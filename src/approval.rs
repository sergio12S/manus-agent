//! Human approval for operations outside the agent's budget.
//!
//! An agent runs with the same OS user and often with a shell, so an approval
//! must be something the agent cannot perform on its own: a tool call, a CLI
//! command, or a clickable dialog would all be forgeable. On macOS the gate is
//! LocalAuthentication (Touch ID or the account password in secure system UI),
//! which neither shell scripts nor UI automation can satisfy.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved { method: String },
    Denied { reason: String },
}

pub trait Approver: Send + Sync {
    /// Block until the human approves or denies `reason`.
    fn request(&self, reason: &str) -> ApprovalOutcome;
    fn describe(&self) -> &'static str;
}

pub fn default_approver() -> Arc<dyn Approver> {
    match std::env::var("MANUS_APPROVAL").as_deref() {
        Ok("none") => Arc::new(NoApprover),
        _ => platform_approver(),
    }
}

/// Refuses everything outside the budget; useful for fully unattended agents.
pub struct NoApprover;

impl Approver for NoApprover {
    fn request(&self, _reason: &str) -> ApprovalOutcome {
        ApprovalOutcome::Denied {
            reason: "approvals are disabled (MANUS_APPROVAL=none)".into(),
        }
    }

    fn describe(&self) -> &'static str {
        "disabled"
    }
}

#[cfg(target_os = "macos")]
fn platform_approver() -> Arc<dyn Approver> {
    Arc::new(macos::DeviceOwner)
}

#[cfg(not(target_os = "macos"))]
fn platform_approver() -> Arc<dyn Approver> {
    Arc::new(NoApprover)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{ApprovalOutcome, Approver};
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Touch ID, Apple Watch, or the macOS account password via system UI.
    pub struct DeviceOwner;

    impl Approver for DeviceOwner {
        fn request(&self, reason: &str) -> ApprovalOutcome {
            let policy = LAPolicy::DeviceOwnerAuthentication;
            let context = unsafe { LAContext::new() };
            if let Err(error) = unsafe { context.canEvaluatePolicy_error(policy) } {
                return ApprovalOutcome::Denied {
                    reason: format!(
                        "device owner authentication unavailable: {}",
                        error.localizedDescription()
                    ),
                };
            }
            let (sender, receiver) = mpsc::channel::<Result<(), String>>();
            let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
                let outcome = if success.as_bool() {
                    Ok(())
                } else if error.is_null() {
                    Err("authentication failed".to_string())
                } else {
                    Err(unsafe { &*error }.localizedDescription().to_string())
                };
                let _ = sender.send(outcome);
            });
            let reason = NSString::from_str(reason);
            unsafe { context.evaluatePolicy_localizedReason_reply(policy, &reason, &reply) };
            match receiver.recv_timeout(Duration::from_secs(180)) {
                Ok(Ok(())) => ApprovalOutcome::Approved {
                    method: "macos_device_owner".into(),
                },
                Ok(Err(reason)) => ApprovalOutcome::Denied { reason },
                Err(_) => {
                    unsafe { context.invalidate() };
                    ApprovalOutcome::Denied {
                        reason: "approval timed out".into(),
                    }
                }
            }
        }

        fn describe(&self) -> &'static str {
            "Touch ID / macOS password"
        }
    }
}

/// Scripted approver for tests.
#[cfg(test)]
pub struct Scripted(pub std::sync::Mutex<Vec<bool>>);

#[cfg(test)]
impl Approver for Scripted {
    fn request(&self, _reason: &str) -> ApprovalOutcome {
        match self.0.lock().unwrap().pop() {
            Some(true) => ApprovalOutcome::Approved {
                method: "test".into(),
            },
            _ => ApprovalOutcome::Denied {
                reason: "test denial".into(),
            },
        }
    }

    fn describe(&self) -> &'static str {
        "scripted"
    }
}
