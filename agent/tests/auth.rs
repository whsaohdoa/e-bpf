//! S-B authentication (§12.2): a missing client certificate and a certificate
//! issued to somebody else must both end the run as fatal `agent.*` codes, not
//! as retryable transport errors.

use agent::client::tonic::TonicClient;
use agent::client::{ClientError, ControlClient};
use agent::error::ServerErrorCode;
use agent::mock::{MockConfig, MockControl};
use agent::pb;

fn config() -> MockConfig {
    MockConfig {
        require_client_cert: true,
        ..MockConfig::default()
    }
}

fn register_request() -> pb::RegisterRequest {
    pb::RegisterRequest {
        machine_id: "machine-1".to_string(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        boot_id: "boot-test".to_string(),
        capabilities: Some(pb::Capabilities {
            config_schema: vec![1],
            default_action: vec![pb::DefaultAction::Pass as i32],
            rules_execution: false,
        }),
    }
}

#[tokio::test]
async fn no_client_certificate_is_fatal_unauthenticated() -> anyhow::Result<()> {
    let mock = MockControl::start(config()).await?;
    let client = TonicClient::connect(&mock.anonymous_client_config())?;

    let error = client
        .register(register_request())
        .await
        .expect_err("the control plane requires a client certificate");

    match &error {
        ClientError::Fatal { code, .. } => {
            assert_eq!(*code, ServerErrorCode::AgentUnauthenticated);
        }
        ClientError::Transport {
            status, message, ..
        } => panic!("a rejected handshake must be fatal, not retryable ({status}: {message})"),
    }
    assert!(error.is_fatal());
    assert!(!error.retryable());

    Ok(())
}

#[tokio::test]
async fn a_certificate_issued_to_another_agent_is_fatal_and_leaves_no_trace() -> anyhow::Result<()>
{
    let mock = MockControl::start(config()).await?;
    let client = TonicClient::connect(&mock.mismatched_client_config())?;

    let error = client
        .register(register_request())
        .await
        .expect_err("the certificate is signed by the CA but names another agent");

    match &error {
        ClientError::Fatal { code, .. } => {
            assert_eq!(*code, ServerErrorCode::AgentIdentityMismatch);
        }
        ClientError::Transport {
            status, message, ..
        } => {
            panic!("a mismatched identity must be fatal ({status}: {message})")
        }
    }

    Ok(())
}

#[tokio::test]
async fn the_accepted_certificate_can_register() -> anyhow::Result<()> {
    let mock = MockControl::start(config()).await?;
    let client = TonicClient::connect(&mock.client_config())?;

    let response = client.register(register_request()).await?;
    assert_eq!(response.agent_id, MockConfig::default().agent_id);
    assert_eq!(response.status, pb::AgentStatus::Active as i32);

    Ok(())
}
