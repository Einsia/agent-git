use super::*;
use agit_peer::access::{Access, Principal, Resource, Rule};

/// The device's own rules for a project id outlive the folder's binding: unbinding the project and
/// binding the same folder again under the same id leaves a member denied it as before, while a
/// machine-wide grant still reaches other projects. The Hub links a removed folder's conversations
/// back to the folder only when the device project id is the same, on the strength of this; an
/// unbind that dropped the rules would let the member in once the folder came back.
#[test]
fn unbinding_a_project_leaves_the_devices_rules_for_it() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let folder = tempfile::tempdir().unwrap();
                let path = folder.path().canonicalize().unwrap();
                let member = Principal {
                    issuer: "https://cloud.example".into(),
                    account_id: "member".into(),
                };
                for (resource, access) in [
                    (Resource::Machine, Access::Control),
                    (Resource::Project("web-exec".into()), Access::Deny),
                ] {
                    crate::rc::cloud::store::grant(Rule {
                        principal: member.clone(),
                        resource,
                        access,
                    })
                    .unwrap();
                }
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let (frames, _frames_rx) = mpsc::channel(8);
                let send = |method: &str, params: serde_json::Value| {
                    let mut frame = Frame::request(method, params);
                    frame.caller = Some(claim("owner", "ws"));
                    frame
                };
                let bind = send(
                    method::PROJECT_BIND,
                    serde_json::json!({"workspace_id":"ws","project_id":"web-exec","local_path":path}),
                );
                let unbind = send(
                    method::PROJECT_UNBIND,
                    serde_json::json!({"workspace_id":"ws","project_id":"web-exec"}),
                );
                let denied = || {
                    crate::rc::cloud::store::policy()
                        .unwrap()
                        .access(&member, None, Some("web-exec"))
                };
                let mut state = daemon.lock().await;
                state.dispatch(&bind, &frames).await.unwrap();
                assert_eq!(denied(), Access::Deny);
                state.dispatch(&unbind, &frames).await.unwrap();
                assert_eq!(denied(), Access::Deny, "unbound");
                state.dispatch(&bind, &frames).await.unwrap();
                assert_eq!(denied(), Access::Deny, "bound again");
                assert_eq!(
                    crate::rc::cloud::store::policy()
                        .unwrap()
                        .access(&member, None, Some("another-project")),
                    Access::Control
                );
            })
    });
}
