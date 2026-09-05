//! Manual integration check. Package/sign this executable with RV's profile
//! before running it. Uses a fresh temporary credential, never a saved VNC password.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    std::thread::spawn(|| -> Result<(), rv_core::StoreError> {
        let id = rv_core::Connection::new("Keychain check", "localhost", 5900).id;
        let result: Result<(), rv_core::StoreError> = (|| {
            rv_core::save_password(id, "temporary test credential")?;
            // Also exercise updates of an existing access-controlled item.
            rv_core::save_password(id, "updated test credential")?;
            println!("Authenticate with Touch ID to read RV's temporary test credential.");
            let password = rv_core::load_password(id)?;
            if password.as_deref() != Some("updated test credential") {
                return Err(rv_core::StoreError::Keyring("round-trip mismatch".into()));
            }
            println!("Protected Keychain create, update, and authenticated read passed.");
            Ok(())
        })();
        let cleanup = rv_core::delete_password(id);
        result?;
        cleanup?;
        println!("Temporary credential removed.");
        Ok(())
    })
    .join()
    .expect("Keychain worker panicked")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This manual check is for macOS Touch ID Keychain access.");
}
