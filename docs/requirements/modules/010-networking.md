## 5. Networking Architecture

This is one of the most important architectural requirements.

The system must support three connection modes.

### Mode A: Direct P2P

Attempt direct peer-to-peer connectivity first.

Investigate:

- STUN.
- ICE.
- UDP hole punching.
- QUIC.
- Other suitable NAT traversal mechanisms.

### Mode B: Automatic Relay Fallback

If direct P2P cannot be established, or the connection quality is unacceptable, automatically switch to a relay server.

The transition must be transparent to users.

Avoid unnecessary session interruption.

### Mode C: User-Selected Relay

Users must be able to configure their own relay servers.

This feature is a major product differentiator.

The relay server must not be hardcoded into the client.

