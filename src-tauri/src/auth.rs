//! Native macOS authentication for destructive actions.
//!
//! `LAPolicy::DeviceOwnerAuthentication` accepts Touch ID/Face ID when
//! available and falls back to the account password. The prompt is deliberately
//! shown before the in-app confirmation dialog, matching the product safety
//! flow: prove who you are, then confirm what will be deleted.

#[cfg(target_os = "macos")]
pub fn authenticate(reason: &str) -> Result<(), String> {
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSError, NSString};
    use objc2_local_authentication::{LAContext, LAPolicy};

    // Keep the context alive until the asynchronous reply arrives.
    let context = unsafe { LAContext::new() };
    let reason = NSString::from_str(reason);
    let (tx, rx) = mpsc::channel::<Option<String>>();

    let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
        let message = if success.as_bool() {
            None
        } else if error.is_null() {
            Some("Authentication was not completed.".to_string())
        } else {
            // The framework description is already localized (for example
            // "Canceled by user").
            Some(unsafe { (*error).localizedDescription().to_string() })
        };
        let _ = tx.send(message);
    });

    unsafe {
        context.evaluatePolicy_localizedReason_reply(
            LAPolicy::DeviceOwnerAuthentication,
            &reason,
            &reply,
        );
    }

    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(None) => Ok(()),
        Ok(Some(message)) => Err(message),
        Err(_) => Err("Authentication timed out.".to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn authenticate(_reason: &str) -> Result<(), String> {
    Err("biometric or password authentication is only available on macOS".to_string())
}
