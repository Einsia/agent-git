//! Scope Keychain authorization dialogs to interactive terminal operations.

use anyhow::Context;
use security_framework::os::macos::keychain::{KeychainUserInteractionLock, SecKeychain};
use std::io::IsTerminal;
use std::sync::Mutex;

// Keychain Services shares the interaction flag across the process. Interactive callers must
// hold the same lock, or they can inherit another operation's temporary no-dialog policy.
static ACCESS: Mutex<()> = Mutex::new(());

trait InteractionControl {
    type Guard;

    fn allowed(&self) -> crate::Result<bool>;
    fn disable(&self) -> crate::Result<Self::Guard>;
}

struct KeychainInteraction;

impl InteractionControl for KeychainInteraction {
    type Guard = KeychainUserInteractionLock;

    fn allowed(&self) -> crate::Result<bool> {
        Ok(SecKeychain::user_interaction_allowed()?)
    }

    fn disable(&self) -> crate::Result<Self::Guard> {
        Ok(SecKeychain::disable_user_interaction()?)
    }
}

fn with_policy<C: InteractionControl, T>(
    control: &C,
    serialization: &Mutex<()>,
    interactive: bool,
    operation: impl FnOnce() -> crate::Result<T>,
) -> crate::Result<T> {
    // The mutex has no mutable payload; unwinding restores the scoped policy before releasing
    // it, so a poisoned lock can still provide exclusion for the next operation.
    let _serialized = serialization
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    // The framework guard enables dialogs on drop. Acquire it only when dialogs were enabled,
    // so an externally disabled policy stays disabled. It drops before the mutex on all paths.
    let _policy = if !interactive
        && control.allowed().context(
            "cannot read the macOS Keychain interaction policy; the credential store was not accessed",
        )?
    {
        Some(control.disable().context(
            "cannot disable macOS Keychain authorization dialogs for a noninteractive command; the credential store was not accessed",
        )?)
    } else {
        None
    };
    operation()
}

pub(super) fn with_access<T>(operation: impl FnOnce() -> crate::Result<T>) -> crate::Result<T> {
    with_policy(
        &KeychainInteraction,
        &ACCESS,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        operation,
    )
}

// These OSStatus values are defined in Security.framework/Headers/SecBase.h. Error messages
// are localized; only the typed platform status can decide which recovery advice applies.
const ERR_SEC_USER_CANCELED: i32 = -128;
const ERR_SEC_NOT_AVAILABLE: i32 = -25291;
const ERR_SEC_NO_DEFAULT_KEYCHAIN: i32 = -25307;
const ERR_SEC_AUTH_FAILED: i32 = -25293;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_SEC_INTERACTION_REQUIRED: i32 = -25315;

fn status(error: &keyring::Error) -> Option<i32> {
    match error {
        keyring::Error::PlatformFailure(inner) | keyring::Error::NoStorageAccess(inner) => inner
            .downcast_ref::<security_framework::base::Error>()
            .map(|error| error.code()),
        _ => None,
    }
}

pub(super) fn requires_authorization(error: &keyring::Error) -> bool {
    matches!(
        status(error),
        Some(
            ERR_SEC_USER_CANCELED
                | ERR_SEC_AUTH_FAILED
                | ERR_SEC_INTERACTION_NOT_ALLOWED
                | ERR_SEC_INTERACTION_REQUIRED
        )
    )
}

pub(super) fn is_unavailable(error: &keyring::Error) -> bool {
    matches!(
        status(error),
        Some(ERR_SEC_NOT_AVAILABLE | ERR_SEC_NO_DEFAULT_KEYCHAIN)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct FakeControl {
        allowed: Arc<AtomicBool>,
        reads: AtomicUsize,
        writes: AtomicUsize,
        fail_read: bool,
        fail_disable: bool,
    }

    impl FakeControl {
        fn new(allowed: bool) -> Self {
            Self {
                allowed: Arc::new(AtomicBool::new(allowed)),
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
                fail_read: false,
                fail_disable: false,
            }
        }
    }

    struct FakeGuard(Arc<AtomicBool>);

    impl Drop for FakeGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl InteractionControl for FakeControl {
        type Guard = FakeGuard;

        fn allowed(&self) -> crate::Result<bool> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail_read, "policy read refused");
            Ok(self.allowed.load(Ordering::SeqCst))
        }

        fn disable(&self) -> crate::Result<Self::Guard> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!self.fail_disable, "policy change refused");
            self.allowed.store(false, Ordering::SeqCst);
            Ok(FakeGuard(Arc::clone(&self.allowed)))
        }
    }

    #[test]
    fn noninteractive_access_disables_dialogs_and_restores_after_success_or_error() {
        for succeeds in [true, false] {
            let control = FakeControl::new(true);
            let lock = Mutex::new(());
            let result = with_policy(&control, &lock, false, || {
                assert!(!control.allowed.load(Ordering::SeqCst));
                assert!(lock.try_lock().is_err());
                anyhow::ensure!(succeeds, "credential access refused");
                Ok(42)
            });
            assert_eq!(result.is_ok(), succeeds);
            assert!(control.allowed.load(Ordering::SeqCst));
            assert_eq!(control.writes.load(Ordering::SeqCst), 1);
            assert!(lock.try_lock().is_ok());
        }
    }

    #[test]
    fn interactive_access_and_preexisting_disabled_policy_are_preserved() {
        for (interactive, initially_allowed) in [(true, true), (true, false), (false, false)] {
            let control = FakeControl::new(initially_allowed);
            let lock = Mutex::new(());
            with_policy(&control, &lock, interactive, || {
                assert_eq!(control.allowed.load(Ordering::SeqCst), initially_allowed);
                assert!(lock.try_lock().is_err());
                Ok(())
            })
            .unwrap();
            assert_eq!(control.allowed.load(Ordering::SeqCst), initially_allowed);
            assert_eq!(control.writes.load(Ordering::SeqCst), 0);
            assert_eq!(
                control.reads.load(Ordering::SeqCst),
                usize::from(!interactive)
            );
        }
    }

    #[test]
    fn policy_errors_fail_before_accessing_credentials() {
        for fail_read in [true, false] {
            let mut control = FakeControl::new(true);
            control.fail_read = fail_read;
            control.fail_disable = !fail_read;
            let lock = Mutex::new(());
            let result: crate::Result<()> = with_policy(&control, &lock, false, || {
                panic!("credentials must not be accessed when policy setup fails")
            });
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("credential store was not accessed")
            );
            assert!(control.allowed.load(Ordering::SeqCst));
            assert!(lock.try_lock().is_ok());
        }
    }

    #[test]
    fn unwinding_restores_the_policy_and_does_not_strand_the_next_access() {
        let control = FakeControl::new(true);
        let lock = Mutex::new(());
        let result = std::panic::catch_unwind(|| {
            let _: crate::Result<()> = with_policy(&control, &lock, false, || {
                assert!(!control.allowed.load(Ordering::SeqCst));
                panic!("credential operation panicked")
            });
        });
        assert!(result.is_err());
        assert!(control.allowed.load(Ordering::SeqCst));
        with_policy(&control, &lock, true, || {
            assert!(control.allowed.load(Ordering::SeqCst));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn framework_policy_is_restored_without_accessing_keychain_entries() {
        with_policy(&KeychainInteraction, &ACCESS, true, || {
            let before = SecKeychain::user_interaction_allowed()?;
            let local_lock = Mutex::new(());
            with_policy(&KeychainInteraction, &local_lock, false, || {
                assert!(!SecKeychain::user_interaction_allowed()?);
                Ok(())
            })?;
            assert_eq!(SecKeychain::user_interaction_allowed()?, before);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn authorization_failures_keep_the_existing_keystore() {
        for code in [
            ERR_SEC_USER_CANCELED,
            ERR_SEC_AUTH_FAILED,
            ERR_SEC_INTERACTION_NOT_ALLOWED,
            ERR_SEC_INTERACTION_REQUIRED,
        ] {
            for platform_failure in [true, false] {
                let inner = Box::new(security_framework::base::Error::from_code(code));
                let error = if platform_failure {
                    keyring::Error::PlatformFailure(inner)
                } else {
                    keyring::Error::NoStorageAccess(inner)
                };
                assert!(requires_authorization(&error));
                assert!(!is_unavailable(&error));
                let rendered = format!(
                    "{:#}",
                    super::super::OsKeyStore::failure("cannot read vault key", error)
                );
                assert!(rendered.contains("rerun this command from a terminal"));
                assert!(rendered.contains("keep the current keystore"));
                assert!(!rendered.contains("agit config secrets.keystore file"));
            }
        }
    }

    #[test]
    fn unavailable_and_unknown_statuses_do_not_get_authorization_guidance() {
        for (code, unavailable) in [
            (ERR_SEC_NOT_AVAILABLE, true),
            (ERR_SEC_NO_DEFAULT_KEYCHAIN, true),
            (-25292, false),
            (-34018, false),
        ] {
            let error = keyring::Error::PlatformFailure(Box::new(
                security_framework::base::Error::from_code(code),
            ));
            assert!(!requires_authorization(&error));
            assert_eq!(is_unavailable(&error), unavailable);
            let rendered = format!(
                "{:#}",
                super::super::OsKeyStore::failure("cannot access vault key", error)
            );
            assert_eq!(
                rendered.contains("agit config secrets.keystore file"),
                unavailable
            );
        }
        let localized = keyring::Error::PlatformFailure(Box::new(std::io::Error::other(
            "User interaction is not allowed. -25308",
        )));
        assert!(!requires_authorization(&localized));
        assert!(!is_unavailable(&localized));
    }
}
