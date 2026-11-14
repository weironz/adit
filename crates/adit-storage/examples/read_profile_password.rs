//! Reads one profile's password from the encrypted credential store and writes
//! it to stdout, so a caller can pipe it into a tool that takes the password
//! on stdin.
//!
//! The RDP probe takes its password on stdin and never in argv, and the
//! credential store cannot be opened from a shell, so something has to bridge
//! the two. This is that bridge: the plaintext is written once to stdout and
//! the caller pipes it straight into a subprocess. It is never logged.
//!
//! Unlike the other acceptance examples in this tree, this one reads the
//! *user's real* store (`C:\Dropbox\adit`), because that is where a real
//! host's password lives. It is therefore deliberately hard to run by
//! accident: the caller must name the exact profile id it wants, and the
//! profile's host must be a private or loopback address, so it cannot be
//! aimed at a third party. The plaintext still leaves the process, so it
//! belongs to a local diagnostic session, not to a shared one.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        return Err("usage: read_profile_password <profile-id>".into());
    }
    // `ProfileId` is a newtype over a UUID, so serde accepts the bare string.
    let id: adit_domain::ProfileId = serde_json::from_str(&format!("\"{}\"", args[1]))?;
    let store = adit_storage::CredentialStore::default();
    let Some(password) = store.load_profile_password(id)? else {
        return Err("no stored password for that profile".into());
    };
    // The profile's host is checked so this example cannot be pointed at a
    // machine the user does not control. The credential store keeps the
    // password keyed to the profile, so refusing here is enough: without the
    // profile there is no password to print.
    //
    // The check is "not on the public internet", not "RFC 1918": a tailnet
    // host (Tailscale, WireGuard) sits in RFC 6598's shared address space,
    // which is exactly the user's own machine and is outside every
    // `is_private` range.
    let profiles = adit_storage::ProfileStore::default().load_profiles()?;
    let profile = profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or("no such profile")?;
    if !adit_domain::is_non_public_host(&profile.host) {
        return Err("refusing to print a credential for a non-private host".into());
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    out.write_all(password.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
