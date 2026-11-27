## 8. Security Architecture

Security must be designed from the beginning.

Requirements:

- End-to-end encryption.
- Mutual authentication.
- Device identity.
- Secure key exchange.
- Secure credential storage.
- Session authorization.
- Optional two-factor authentication.
- Device pairing.
- Access control.
- Session expiration.
- Replay protection.
- Protection against unauthorized relay impersonation.
- Secure update mechanism.
- Audit logs.

The relay server must not be able to decrypt remote desktop sessions.

Do not store passwords or authentication secrets in plaintext.

Provide clear controls for revoking trusted devices and terminating active sessions.

Implement security testing before public release.

