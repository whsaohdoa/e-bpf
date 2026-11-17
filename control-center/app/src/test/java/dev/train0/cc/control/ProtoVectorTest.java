package dev.train0.cc.control;

import static org.assertj.core.api.Assertions.assertThat;

import com.google.protobuf.TextFormat;
import io.probe.control.agent.v1.ConfigDocument;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;

/**
 * Contract §6.4 / appendix A: the control plane must reproduce the exact bytes an agent
 * compares against, otherwise {@code content_sha256} never matches. The fixtures are parsed
 * with {@link TextFormat} instead of being built field by field, so the vector exercises the
 * real encoder rather than a hand-written builder that could silently disagree with it.
 */
class ProtoVectorTest {

    private static final HexFormat HEX = HexFormat.of();

    @Test
    void docAEncodesToTheContractVector() throws IOException {
        ConfigDocument doc = readDocument("/proto/doc_a.textproto");
        byte[] bytes = doc.toByteArray();

        assertThat(HEX.formatHex(bytes)).isEqualTo("080110904e18d00f20182840");
        assertThat(bytes).hasSize(12);
        assertThat(sha256Hex(bytes))
                .isEqualTo("971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05");
    }

    @Test
    void docBEncodesToTheContractVector() throws IOException {
        ConfigDocument doc = readDocument("/proto/doc_b.textproto");
        byte[] bytes = doc.toByteArray();

        // Guard against a fixture that parses into an empty document: length 63 alone would not
        // catch a dropped rules block.
        assertThat(doc.getRulesCount()).isEqualTo(1);
        assertThat(bytes).hasSize(63);
        assertThat(HEX.formatHex(bytes)).startsWith("080110904e18d00f201828403a31").endsWith("3001");
        assertThat(sha256Hex(bytes))
                .isEqualTo("c3e4eea7f545643b966956fbea32fbbc3fb44acc019999dd5ed9552940b3d30f");
    }

    private static ConfigDocument readDocument(String resource) throws IOException {
        try (InputStream in = ProtoVectorTest.class.getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("missing test resource " + resource);
            }
            String text = new String(in.readAllBytes(), StandardCharsets.UTF_8);
            return TextFormat.parse(text, ConfigDocument.class);
        }
    }

    private static String sha256Hex(byte[] bytes) {
        try {
            return HEX.formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("SHA-256 is required by the JVM spec", e);
        }
    }
}
