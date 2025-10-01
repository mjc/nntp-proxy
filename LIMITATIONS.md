# NNTP Proxy Limitations

## Stateless Proxy Mode

This proxy operates in **stateless mode**, which means it does not maintain NNTP session state like current newsgroup selection or article pointers. This design choice enables:

- **Simpler architecture** - No state synchronization needed
- **Better scalability** - Future multiplexing capabilities
- **Reduced complexity** - Easier to maintain and debug

## Unsupported Commands

The following NNTP commands are **rejected** with error `500 Command not supported by this proxy (stateless proxy mode)`:

### Group Context Commands
- `GROUP <newsgroup>` - Selecting a newsgroup
- `NEXT` - Moving to next article
- `LAST` - Moving to previous article
- `LISTGROUP` - Listing articles in a group

### Article Retrieval by Number
- `ARTICLE <number>` - Requires current group context
- `HEAD <number>` - Requires current group context
- `BODY <number>` - Requires current group context
- `STAT <number>` - Requires current group context
- `ARTICLE` (no argument) - Requires current article pointer

### Overview/Header Commands
- `XOVER <range>` - Requires current group context
- `OVER <range>` - Requires current group context
- `XHDR <header> <range>` - Requires current group context
- `HDR <header> <range>` - Requires current group context

## Supported Commands

The following commands work normally through the proxy:

### Authentication
- `AUTHINFO USER <username>` - Intercepted and handled locally
- `AUTHINFO PASS <password>` - Intercepted and handled locally

### Article Retrieval by Message-ID
- `ARTICLE <message-id>` - Globally unique, stateless
- `HEAD <message-id>` - Globally unique, stateless
- `BODY <message-id>` - Globally unique, stateless
- `STAT <message-id>` - Globally unique, stateless

**Example:** `ARTICLE <12345@news.example.com>`

### Metadata Commands
- `LIST [variant]` - List newsgroups, active groups, etc.
- `HELP` - Server help
- `DATE` - Server date/time
- `CAPABILITIES` - Server capabilities
- `NEWGROUPS <date> <time>` - New groups since date
- `NEWNEWS <groups> <date> <time>` - New articles since date
- `MODE READER` - Switch to reader mode
- `POST` - Post an article
- `QUIT` - End session

## Workarounds for Stateful Commands

If you need to access articles by number:

1. **Use message-IDs instead** - If your client supports it, use message-ID based retrieval
2. **Direct connection** - Connect directly to the backend server for stateful operations
3. **Different proxy** - Use a full-featured NNTP proxy that maintains state

## Why This Design?

Traditional NNTP proxies maintain a 1:1 mapping between client and backend connections, forwarding all commands including stateful ones. This proxy is designed to eventually support **connection multiplexing**, where multiple clients can share backend connections.

Multiplexing requires either:
- **No state** (our approach) - Simple, efficient
- **State synchronization** - Complex, error-prone, slower

By rejecting stateful commands, we:
- Keep the architecture simple and maintainable
- Enable future multiplexing of stateless commands
- Reduce backend connection count for metadata operations
- Maintain compatibility with clients that use message-IDs

## Client Compatibility

This proxy works best with:
- ✅ Clients that primarily use message-IDs for article retrieval
- ✅ Indexing/search tools that query metadata
- ✅ Monitoring tools that use LIST commands
- ❌ Traditional newsreaders that rely on GROUP/NEXT/LAST navigation

Most traditional newsreaders (tin, slrn, Thunderbird, etc.) **will not work** with this proxy as they depend heavily on GROUP-based navigation.

## Future Enhancements

Potential future features:
- **Hybrid mode** - Pass stateful commands to dedicated connections
- **Session affinity** - Maintain client-to-backend mapping for stateful operations
- **Multiplexing** - Share backend connections for stateless commands only
