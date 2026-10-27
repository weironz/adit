use super::*;

impl SessionManager {
    /// UI resolves these through its already-unlocked CredentialStore. Keep
    /// them out of ConnectionProfile, catalog serialization and reconnect logs.
    pub fn set_jump_credential(&mut self, owner: ProfileId, credential: JumpCredential) {
        self.jump_credentials.insert(owner, credential);
    }

    pub fn clear_jump_credentials(&mut self) {
        self.jump_credentials.clear();
    }

    pub(super) fn resolve_jump_credentials(
        &self,
        profile: &ConnectionProfile,
    ) -> Result<JumpCredentials, SessionError> {
        let mut resolved = JumpCredentials::new();
        for hop in &profile.jumps {
            if let Some(owner) = hop.auth_profile {
                if !self
                    .profiles
                    .iter()
                    .any(|p| p.id == owner && p.protocol.is_ssh_based())
                {
                    return Err(SshError::MissingJumpCredential(owner).into());
                }
                let credential = self
                    .jump_credentials
                    .get(&owner)
                    .ok_or(SshError::MissingJumpCredential(owner))?;
                resolved.insert(owner, credential.clone());
            }
        }
        Ok(resolved)
    }
}

/// Reuse the target's existing auth-method mapping, including distinct vault
/// passphrases. OTP answers are intentionally absent from this constructor.
pub fn jump_credential(
    profile: &ConnectionProfile,
    password: String,
    passphrase: String,
) -> JumpCredential {
    JumpCredential {
        username: profile.username.clone(),
        auth: auth_options_for_profile(profile, &password, &passphrase),
        password,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_jump_ids_resolve_only_their_owners_and_preserve_auth_method() {
        let mut target = ConnectionProfile::new("target", "target.invalid", 22, "target-user");
        let mut key_owner = ConnectionProfile::new("key-owner", "unused.invalid", 22, "key-user");
        key_owner.auth_method = AuthMethod::Key;
        key_owner.identity_file = "fixture-only.key".into();
        let mut agent_owner =
            ConnectionProfile::new("agent-owner", "unused.invalid", 22, "agent-user");
        agent_owner.auth_method = AuthMethod::Agent;
        let mut first = JumpHop::parse("bastion-one.invalid").unwrap();
        first.auth_profile = Some(key_owner.id);
        let mut second = JumpHop::parse("bastion-two.invalid").unwrap();
        second.auth_profile = Some(agent_owner.id);
        target.jumps = vec![first, second];
        let mut manager = SessionManager::with_profiles(vec![
            target.clone(),
            key_owner.clone(),
            agent_owner.clone(),
        ]);
        assert!(manager.resolve_jump_credentials(&target).is_err());
        manager.set_jump_credential(
            key_owner.id,
            jump_credential(&key_owner, "".into(), "SYNTHETIC-KEY-SECRET".into()),
        );
        manager.set_jump_credential(
            agent_owner.id,
            jump_credential(&agent_owner, "".into(), "".into()),
        );
        let resolved = manager.resolve_jump_credentials(&target).unwrap();
        assert!(!resolved[&key_owner.id].auth.try_agent);
        assert!(resolved[&key_owner.id].auth.identity_file.is_some());
        assert!(resolved[&agent_owner.id].auth.try_agent);
        assert!(!resolved[&agent_owner.id].auth.try_password);
        let json = serde_json::to_string(&target).unwrap();
        assert!(!json.contains("SYNTHETIC-KEY-SECRET"));
        assert!(json.contains(&key_owner.id.to_string()));
        let roundtrip: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.jumps, target.jumps);
        let old: JumpHop =
            serde_json::from_str(r#"{"host":"old.invalid","port":22,"username":"legacy"}"#)
                .unwrap();
        assert_eq!(old.auth_profile, None);
        manager.clear_jump_credentials();
        assert!(manager.resolve_jump_credentials(&target).is_err());
    }
}
