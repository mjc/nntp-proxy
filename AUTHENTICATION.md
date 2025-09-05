# NNTP Authentication Support

## Overview

The NNTP proxy now supports authentication for backend servers. You can configure username and password credentials for servers that require authentication while leaving them optional for servers that don't.

## Configuration Format

Authentication credentials are optional fields in the server configuration:

```toml
[[servers]]
host = "news.example.com"
port = 119
name = "Public News Server"
# No authentication needed - username and password are omitted

[[servers]]
host = "secure.example.com"
port = 563
name = "Secure News Server"
username = "your_username"
password = "your_password"

[[servers]]
host = "another.example.com"
port = 119
name = "Another Server"
# Optional authentication (can be commented out when not needed)
# username = "optional_user"
# password = "optional_pass"
```

## Configuration Fields

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `host` | String | Yes | Server hostname or IP address |
| `port` | Number | Yes | Server port number (usually 119 or 563) |
| `name` | String | Yes | Human-readable server name for logging |
| `username` | String | No | Username for NNTP authentication |
| `password` | String | No | Password for NNTP authentication |

## Authentication Behavior

- **No credentials**: If `username` and `password` are omitted or set to `null`, the proxy will connect to the backend server without authentication
- **With credentials**: If both `username` and `password` are provided, the proxy will use these credentials when connecting to the backend server
- **Partial credentials**: If only one of `username` or `password` is provided, the proxy will treat it as no authentication (both fields must be present for authentication to be used)

## Security Considerations

1. **File Permissions**: Ensure your `config.toml` file has appropriate permissions (e.g., `chmod 600 config.toml`) to protect stored passwords
2. **Plain Text Storage**: Currently passwords are stored in plain text in the configuration file. Consider using environment variables or encrypted storage for production deployments
3. **Network Security**: Use NNTPS (port 563) when possible for encrypted connections to backend servers

## Example Configurations

### Mixed Authentication Setup
```toml
# Public server - no auth needed
[[servers]]
host = "news.gmane.io"
port = 119
name = "Gmane News Archive"

# ISP server - requires authentication
[[servers]]
host = "news.myisp.com"
port = 119
name = "ISP News Server"
username = "myaccount"
password = "mypassword"

# Secure server - with SSL and auth
[[servers]]
host = "secure-news.example.com"  
port = 563
name = "Secure News Server"
username = "premium_user"
password = "secure_password"
```

### Environment Variable Support (Future Enhancement)
While not currently supported, a future version might support environment variable substitution:

```toml
[[servers]]
host = "secure.example.com"
port = 563
name = "Secure Server"
username = "${NNTP_USERNAME}"
password = "${NNTP_PASSWORD}"
```

## Testing Authentication

To test your authentication setup:

1. Configure your servers in `config.toml` with appropriate credentials
2. Start the proxy: `cargo run -- --port 8119`
3. Connect with an NNTP client to `localhost:8119`
4. Check the logs to ensure successful backend connections

The proxy will log connection attempts and any authentication failures, making it easy to debug configuration issues.

## Implementation Details

The authentication support is implemented in the `ServerConfig` structure with optional `username` and `password` fields using Rust's `Option<String>` type. The fields are automatically omitted from TOML serialization when they are `None`, keeping configuration files clean.

The TOML serialization uses `#[serde(skip_serializing_if = "Option::is_none")]` to ensure that `None` values don't appear in the generated configuration files, maintaining backwards compatibility with existing configurations.
