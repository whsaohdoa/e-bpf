package dev.train0.cc.control;

import com.google.protobuf.ByteString;
import com.google.protobuf.Timestamp;
import dev.train0.cc.control.repo.AckRepository;
import dev.train0.cc.control.repo.AgentRepository;
import dev.train0.cc.control.repo.ConfigRepository;
import dev.train0.cc.control.repo.ReportRepository;
import io.grpc.Status;
import io.grpc.stub.StreamObserver;
import io.probe.control.agent.v1.AckConfigRequest;
import io.probe.control.agent.v1.AckConfigResponse;
import io.probe.control.agent.v1.AckResult;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.AgentStatus;
import io.probe.control.agent.v1.Capabilities;
import io.probe.control.agent.v1.ConfigEnvelope;
import io.probe.control.agent.v1.ConfigNotification;
import io.probe.control.agent.v1.ConfigState;
import io.probe.control.agent.v1.DefaultAction;
import io.probe.control.agent.v1.GetConfigRequest;
import io.probe.control.agent.v1.GetConfigResponse;
import io.probe.control.agent.v1.GetCursorRequest;
import io.probe.control.agent.v1.GetCursorResponse;
import io.probe.control.agent.v1.HeartbeatRequest;
import io.probe.control.agent.v1.HeartbeatResponse;
import io.probe.control.agent.v1.RegisterRequest;
import io.probe.control.agent.v1.RegisterResponse;
import io.probe.control.agent.v1.RejectedWindow;
import io.probe.control.agent.v1.ReportWindowsRequest;
import io.probe.control.agent.v1.ReportWindowsResponse;
import io.probe.control.agent.v1.WatchRequest;
import io.probe.control.agent.v1.WindowReport;
import java.time.Instant;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Optional;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.grpc.server.service.GrpcService;

/**
 * Agent-facing gRPC service of the control plane.
 *
 * <p>Each mutating RPC delegates the whole transaction to one repository method: the response
 * message is assembled only after that method returns, which means after the commit - a client can
 * never observe a success for a write that rolled back. The repository layer owns JDBC, this class
 * owns the protocol: request validation, error codes and protobuf assembly.
 *
 * <p>Identity comes exclusively from {@link AgentPrincipal} published by the mTLS interceptor;
 * nothing in the request payload influences which agent row is touched.
 */
@GrpcService
@ConditionalOnProperty(name = "cc.role", havingValue = "control")
public class AgentControlService extends AgentControlGrpc.AgentControlImplBase {

    /** Heartbeat interval while the agent has not applied the newest config yet. */
    static final int LAGGING_HEARTBEAT_INTERVAL_MS = 1000;

    /** Heartbeat interval once the agent is up to date. */
    static final int IDLE_HEARTBEAT_INTERVAL_MS = 5000;

    /** Hard limit from the contract: one ReportWindows call carries at most 1000 windows. */
    static final int MAX_WINDOWS_PER_BATCH = 1000;

    /** Error code attached to a rejected window in {@code ReportWindowsResponse}. */
    static final String WINDOW_TOO_OLD = "report.window_too_old";

    /** Train 0 control plane view: schema 1, pass-only forwarding, no rule enforcement. */
    private static final Capabilities EFFECTIVE_CAPABILITIES = Capabilities.newBuilder()
            .addConfigSchema(1)
            .addDefaultAction(DefaultAction.DEFAULT_ACTION_PASS)
            .setRulesExecution(false)
            .build();

    private static final int MAX_ERROR_DETAIL_CHARS = 512;

    private final AgentRepository agents;
    private final AckRepository acks;
    private final ReportRepository reports;
    private final ConfigRepository configs;

    public AgentControlService(AgentRepository agents, AckRepository acks, ReportRepository reports,
            ConfigRepository configs) {
        this.agents = agents;
        this.acks = acks;
        this.reports = reports;
        this.configs = configs;
    }

    @Override
    public void register(RegisterRequest request, StreamObserver<RegisterResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        String machineId = request.getMachineId();
        if (machineId.isBlank()) {
            throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "request.invalid",
                    "machine_id must not be blank");
        }

        AgentRepository.RegisterOutcome outcome = agents.register(
                principal.agentId(), machineId, blankToNull(request.getAgentVersion()),
                blankToNull(request.getBootId()), capabilitiesJson(request.getCapabilities()));
        if (outcome == null) {
            throw new IllegalStateException("agent row vanished: " + principal.agentId());
        }
        if (outcome.machineConflict()) {
            throw new AgentRpcException(Status.Code.FAILED_PRECONDITION, "agent.machine_conflict",
                    "agent is already bound to a different machine_id");
        }

        ConfigRepository.DesiredConfig desired = outcome.desired();
        RegisterResponse response = RegisterResponse.newBuilder()
                .setAgentId(principal.agentId())
                .setMachineId(outcome.machineId())
                .setGroupId(emptyIfNull(outcome.groupId()))
                .setStatus(agentStatus(outcome.status()))
                .setDesiredVersion(desired == null ? 0 : desired.version())
                .setDesiredSha256(desired == null ? ByteString.EMPTY : shaBytes(desired.contentSha256()))
                .setHeartbeatIntervalMs(LAGGING_HEARTBEAT_INTERVAL_MS)
                .setIdleHeartbeatIntervalMs(IDLE_HEARTBEAT_INTERVAL_MS)
                .setServerTime(serverTime())
                .setEffectiveCapabilities(EFFECTIVE_CAPABILITIES)
                .build();
        responseObserver.onNext(response);
        responseObserver.onCompleted();
    }

    @Override
    public void heartbeat(HeartbeatRequest request, StreamObserver<HeartbeatResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        AgentRepository.HeartbeatOutcome outcome = agents.heartbeat(principal.agentId(),
                request.getCurrentVersion(), configStateText(request.getConfigState()));
        if (outcome == null) {
            throw new IllegalStateException("agent row vanished: " + principal.agentId());
        }

        ConfigRepository.DesiredConfig desired = outcome.desired();
        boolean lagging = desired != null && desired.version() > request.getCurrentVersion();
        HeartbeatResponse response = HeartbeatResponse.newBuilder()
                .setDesiredVersion(desired == null ? 0 : desired.version())
                .setDesiredSha256(desired == null ? ByteString.EMPTY : shaBytes(desired.contentSha256()))
                .setHeartbeatIntervalMs(
                        lagging ? LAGGING_HEARTBEAT_INTERVAL_MS : IDLE_HEARTBEAT_INTERVAL_MS)
                .setIdleHeartbeatIntervalMs(IDLE_HEARTBEAT_INTERVAL_MS)
                .setServerTime(serverTime())
                .build();
        responseObserver.onNext(response);
        responseObserver.onCompleted();
    }

    @Override
    public void getConfig(GetConfigRequest request, StreamObserver<GetConfigResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        // A pending agent has no group assignment that can be trusted with config content yet:
        // answering "up to date" is the only safe answer and leaks nothing about other groups.
        if (principal.pending()) {
            responseObserver.onNext(GetConfigResponse.newBuilder().setUpToDate(true).build());
            responseObserver.onCompleted();
            return;
        }

        ConfigRepository.DesiredConfig desired = configs.latest(principal.groupId());
        if (desired == null || desired.version() <= request.getCurrentVersion()) {
            responseObserver.onNext(GetConfigResponse.newBuilder().setUpToDate(true).build());
            responseObserver.onCompleted();
            return;
        }

        ConfigEnvelope envelope = ConfigEnvelope.newBuilder()
                .setVersion(desired.version())
                .setSchemaVersion(desired.schemaVersion())
                .setContentSha256(shaBytes(desired.contentSha256()))
                .setDocument(ByteString.copyFrom(desired.document()))
                .setPublishedAt(fromMillis(desired.publishedAt().toEpochMilli()))
                .build();
        responseObserver.onNext(GetConfigResponse.newBuilder()
                .setUpToDate(false)
                .setConfig(envelope)
                .build());
        responseObserver.onCompleted();
    }

    @Override
    public void ackConfig(AckConfigRequest request, StreamObserver<AckConfigResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        String result = ackResultText(request.getResult());

        AckRepository.AckOutcome outcome = acks.record(
                principal.agentId(), request.getVersion(), request.getContentSha256().toByteArray(),
                result, instantOrNull(request.hasAppliedAt(), request.getAppliedAt()),
                request.getApplyMs(), request.getDegradedFeaturesList().toArray(String[]::new),
                blankToNull(request.getErrorCode()),
                truncate(blankToNull(request.getErrorDetail()), MAX_ERROR_DETAIL_CHARS));
        if (outcome == null) {
            throw new IllegalStateException("agent row vanished: " + principal.agentId());
        }
        switch (outcome.status()) {
            case VERSION_UNKNOWN -> throw new AgentRpcException(Status.Code.FAILED_PRECONDITION,
                    "ack.version_unknown", "acked version is ahead of the newest published version");
            case INVALID_CONTENT_HASH -> throw new AgentRpcException(Status.Code.INVALID_ARGUMENT,
                    "request.invalid", "content_sha256 must be the 32 bytes of the acked version");
            case RECORDED -> {
                // Recorded; the response is assembled below from the committed state.
            }
        }

        AckConfigResponse response = AckConfigResponse.newBuilder()
                .setRecorded(true)
                .setDesiredVersion(outcome.desiredVersion())
                .setReceivedAt(outcome.receivedAt() == null ? Timestamp.getDefaultInstance()
                        : fromMillis(outcome.receivedAt().toEpochMilli()))
                .setServerTime(serverTime())
                .build();
        responseObserver.onNext(response);
        responseObserver.onCompleted();
    }

    @Override
    public void reportWindows(ReportWindowsRequest request,
            StreamObserver<ReportWindowsResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        List<WindowReport> windows = request.getWindowsList();
        if (windows.size() > MAX_WINDOWS_PER_BATCH) {
            throw new AgentRpcException(Status.Code.RESOURCE_EXHAUSTED, "report.batch_too_large",
                    "batch carries " + windows.size() + " windows, maximum is "
                            + MAX_WINDOWS_PER_BATCH);
        }
        validateWindowShape(windows);

        ReportRepository.Intake intake = reports.recordWindows(principal.agentId(),
                toPayloads(windows));

        ReportWindowsResponse.Builder response = ReportWindowsResponse.newBuilder()
                .setAccepted(intake.accepted())
                .setDuplicates(intake.duplicates())
                .setRejected(intake.rejected())
                .setLastWindowId(intake.lastWindowId())
                .setServerTime(serverTime());
        for (long windowId : intake.rejectedWindowIds()) {
            response.addRejectedWindows(RejectedWindow.newBuilder()
                    .setWindowId(windowId)
                    .setErrorCode(WINDOW_TOO_OLD));
        }
        responseObserver.onNext(response.build());
        responseObserver.onCompleted();
    }

    @Override
    public void getCursor(GetCursorRequest request, StreamObserver<GetCursorResponse> responseObserver) {
        AgentPrincipal principal = requirePrincipal();
        Optional<Long> lastWindowId = reports.maxWindowId(principal.agentId());
        GetCursorResponse.Builder response = GetCursorResponse.newBuilder().setServerTime(serverTime());
        // Absent means "this agent never delivered a window"; zero would be a valid window id.
        lastWindowId.ifPresent(response::setLastWindowId);
        responseObserver.onNext(response.build());
        responseObserver.onCompleted();
    }

    @Override
    public void watch(WatchRequest request, StreamObserver<ConfigNotification> responseObserver) {
        // Train 0 has no push channel; the agent falls back to heartbeat polling (design §6.8).
        responseObserver.onError(Status.UNIMPLEMENTED
                .withDescription("no push channel: Watch is not implemented by this control plane")
                .asRuntimeException());
    }

    private static AgentPrincipal requirePrincipal() {
        AgentPrincipal principal = AgentPrincipal.CONTEXT_KEY.get();
        if (principal == null) {
            throw new IllegalStateException("agent identity interceptor did not run");
        }
        return principal;
    }

    /**
     * Batch-level validation from design §6.5: both failures reject the whole batch before any row
     * is written, so the caller sees either a total failure or the complete per-window accounting.
     */
    private static void validateWindowShape(List<WindowReport> windows) {
        long previousWindowId = 0;
        for (WindowReport window : windows) {
            if (window.getWindowId() <= 0 || window.getWindowMs() <= 0
                    || window.getWindowId() <= previousWindowId) {
                throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "report.malformed",
                        "window_id and window_ms must be positive and window_id strictly increasing");
            }
            previousWindowId = window.getWindowId();
        }
    }

    private static List<ReportRepository.WindowPayload> toPayloads(List<WindowReport> windows) {
        List<ReportRepository.WindowPayload> payloads = new ArrayList<>(windows.size());
        for (WindowReport window : windows) {
            List<ReportRepository.AggregatePayload> aggregates = new ArrayList<>(
                    window.getAggregatesCount());
            for (var aggregate : window.getAggregatesList()) {
                aggregates.add(new ReportRepository.AggregatePayload(
                        aggregate.getKeyHash().toByteArray(), aggregate.getPackets(),
                        aggregate.getFirstSeenOffsetNs(), aggregate.getLastSeenOffsetNs(),
                        aggregate.getSampleDropped(), aggregate.getDegraded()));
            }
            List<ReportRepository.SamplePayload> samples = new ArrayList<>(window.getSamplesCount());
            for (var sample : window.getSamplesList()) {
                samples.add(new ReportRepository.SamplePayload(sample.getKeyHash().toByteArray(),
                        sample.getFiveTuple(), sample.getTsOffsetNs()));
            }
            payloads.add(new ReportRepository.WindowPayload(window.getWindowId(),
                    window.getWindowMs(), window.getComplete(),
                    instantOrNull(window.hasWindowStart(), window.getWindowStart()),
                    window.getTruncated(), aggregates, samples));
        }
        return payloads;
    }

    private static String ackResultText(AckResult result) {
        return switch (result) {
            case ACK_RESULT_APPLIED -> "applied";
            case ACK_RESULT_REJECTED -> "rejected";
            case ACK_RESULT_FAILED -> "failed";
            case ACK_RESULT_UNSPECIFIED, UNRECOGNIZED -> throw new AgentRpcException(
                    Status.Code.INVALID_ARGUMENT, "request.invalid",
                    "result must not be ACK_RESULT_UNSPECIFIED");
        };
    }

    private static String configStateText(ConfigState configState) {
        return switch (configState) {
            case CONFIG_STATE_MISSING -> "missing";
            case CONFIG_STATE_APPLYING -> "applying";
            case CONFIG_STATE_APPLIED -> "applied";
            case CONFIG_STATE_REJECTED -> "rejected";
            case CONFIG_STATE_UNSPECIFIED, UNRECOGNIZED -> null;
        };
    }

    private static AgentStatus agentStatus(String status) {
        if (status == null) {
            return AgentStatus.AGENT_STATUS_UNSPECIFIED;
        }
        return switch (status) {
            case AgentPrincipal.STATUS_ACTIVE -> AgentStatus.AGENT_STATUS_ACTIVE;
            case AgentPrincipal.STATUS_PENDING -> AgentStatus.AGENT_STATUS_PENDING;
            case AgentPrincipal.STATUS_REVOKED -> AgentStatus.AGENT_STATUS_REVOKED;
            default -> AgentStatus.AGENT_STATUS_UNSPECIFIED;
        };
    }

    /** Serializes the client capabilities into the {@code jsonb} column without an ObjectMapper. */
    private static String capabilitiesJson(Capabilities capabilities) {
        StringBuilder json = new StringBuilder("{\"config_schema\":[");
        for (int i = 0; i < capabilities.getConfigSchemaCount(); i++) {
            if (i > 0) {
                json.append(',');
            }
            json.append(capabilities.getConfigSchema(i));
        }
        json.append("],\"default_action\":[");
        for (int i = 0; i < capabilities.getDefaultActionCount(); i++) {
            if (i > 0) {
                json.append(',');
            }
            json.append(capabilities.getDefaultAction(i).getNumber());
        }
        return json.append("],\"rules_execution\":")
                .append(capabilities.getRulesExecution())
                .append('}')
                .toString();
    }

    private static ByteString shaBytes(String sha256Hex) {
        return ByteString.copyFrom(HexFormat.of().parseHex(sha256Hex));
    }

    private static Instant instantOrNull(boolean present, Timestamp timestamp) {
        return present ? Instant.ofEpochSecond(timestamp.getSeconds(), timestamp.getNanos()) : null;
    }

    /** Current control-plane time as a proto timestamp; protobuf-java-util is not a dependency. */
    private static Timestamp serverTime() {
        return fromMillis(System.currentTimeMillis());
    }

    private static Timestamp fromMillis(long millis) {
        return Timestamp.newBuilder()
                .setSeconds(Math.floorDiv(millis, 1000L))
                .setNanos((int) Math.floorMod(millis, 1000L) * 1_000_000)
                .build();
    }

    private static String blankToNull(String value) {
        return value == null || value.isBlank() ? null : value;
    }

    private static String emptyIfNull(String value) {
        return value == null ? "" : value;
    }

    private static String truncate(String value, int maxChars) {
        return value != null && value.length() > maxChars ? value.substring(0, maxChars) : value;
    }
}
