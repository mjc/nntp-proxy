# Code Duplication Analysis - Command Handling

## The Problem

`CommandHandler::handle_command()` is called in 3 places, and each one has **nearly identical** code to handle the result:

### 1. **src/session/handlers/per_command.rs** (lines 126-165)
```rust
let action = CommandHandler::handle_command(&command);

match action {
    CommandAction::InterceptAuth(auth_action) => {
        let bytes_written = self.auth_handler.handle_auth_command(auth_action, &mut client_write).await?;
        backend_to_client_bytes.add(bytes_written);
    }
    CommandAction::Reject(reason) => {
        warn!("Rejecting command from client {}: {} ({})", self.client_addr, trimmed, reason);
        client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
        backend_to_client_bytes.add(COMMAND_NOT_SUPPORTED_STATELESS.len());
    }
    CommandAction::ForwardStateless => {
        // Route to backend and execute
        self.route_and_execute_command(...).await?;
    }
}
```

### 2. **src/session/handlers/standard.rs** (lines 58-69)
```rust
match CommandHandler::handle_command(&line) {
    CommandAction::InterceptAuth(auth_action) => {
        let bytes_written = self.auth_handler.handle_auth_command(auth_action, &mut client_write).await?;
        backend_to_client_bytes += bytes_written as u64;
        debug!("Intercepted auth command for client {}", self.client_addr);
    }
    CommandAction::Reject(_reason) => {
        warn!("Rejecting command from client {}: {}", self.client_addr, trimmed);
        client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
        backend_to_client_bytes += COMMAND_NOT_SUPPORTED_STATELESS.len() as u64;
    }
    CommandAction::ForwardStateless => {
        // Write directly to backend stream
        backend_write.write_all(line.as_bytes()).await?;
        client_to_backend_bytes += line.len() as u64;
    }
}
```

### 3. **src/cache/session.rs** (lines 84-102)
```rust
match CommandHandler::handle_command(&line) {
    CommandAction::InterceptAuth(auth_action) => {
        use crate::auth::AuthHandler;
        use crate::command::AuthAction;
        let response = match auth_action {
            AuthAction::RequestPassword => AuthHandler::user_response(),  // OLD STATIC WAY
            AuthAction::AcceptAuth => AuthHandler::pass_response(),
        };
        client_write.write_all(response).await?;
        backend_to_client_bytes += response.len() as u64;
    }
    CommandAction::Reject(_reason) => {
        client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
        backend_to_client_bytes += COMMAND_NOT_SUPPORTED_STATELESS.len() as u64;
    }
    CommandAction::ForwardStateless => {
        // Forward to backend and cache response
        backend_write.write_all(line.as_bytes()).await?;
        client_to_backend_bytes += line.len() as u64;
        // ... caching logic
    }
}
```

## What's Duplicated

### Always the Same:
1. ✅ **InterceptAuth handling** - call auth handler, write response, track bytes
2. ✅ **Reject handling** - send error, track bytes

### Different per handler:
3. ❌ **ForwardStateless** - each handler does different thing:
   - per_command: routes to backend via router
   - standard: writes directly to backend stream  
   - cache: writes to backend + caches response

## Proposed Refactoring

### Option 1: Extract to ClientSession method
```rust
impl ClientSession {
    async fn execute_command_action<F, Fut>(
        &self,
        action: CommandAction,
        client_write: &mut impl AsyncWriteExt,
        backend_to_client_bytes: &mut impl AddAssign<usize>,
        forward_handler: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        match action {
            CommandAction::InterceptAuth(auth_action) => {
                let bytes = self.auth_handler.handle_auth_command(auth_action, client_write).await?;
                *backend_to_client_bytes += bytes;
            }
            CommandAction::Reject(reason) => {
                warn!("Rejecting command: {}", reason);
                client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
                *backend_to_client_bytes += COMMAND_NOT_SUPPORTED_STATELESS.len();
            }
            CommandAction::ForwardStateless => {
                forward_handler().await?;
            }
        }
        Ok(())
    }
}
```

Then usage becomes:
```rust
let action = CommandHandler::handle_command(&command);
self.execute_command_action(action, &mut client_write, &mut backend_to_client_bytes, || async {
    // Handler-specific forwarding logic
    self.route_and_execute_command(...).await
}).await?;
```

### Option 2: Command executor trait
```rust
pub trait CommandExecutor {
    async fn execute_auth(&mut self, action: AuthAction) -> Result<usize>;
    async fn execute_reject(&mut self, reason: &str) -> Result<usize>;
    async fn execute_forward(&mut self, command: &str) -> Result<()>;
}

impl ClientSession {
    async fn handle_command_with_executor<E: CommandExecutor>(
        &self,
        command: &str,
        executor: &mut E,
    ) -> Result<()> {
        match CommandHandler::handle_command(command) {
            CommandAction::InterceptAuth(action) => {
                executor.execute_auth(action).await?;
            }
            CommandAction::Reject(reason) => {
                executor.execute_reject(reason).await?;
            }
            CommandAction::ForwardStateless => {
                executor.execute_forward(command).await?;
            }
        }
        Ok(())
    }
}
```

### Option 3: Simplest - just extract auth + reject
```rust
impl ClientSession {
    async fn handle_intercepted_command(
        &self,
        action: CommandAction,
        client_write: &mut impl AsyncWriteExt,
    ) -> Result<Option<usize>> {  // Returns Some(bytes) if handled, None if ForwardStateless
        match action {
            CommandAction::InterceptAuth(auth_action) => {
                let bytes = self.auth_handler.handle_auth_command(auth_action, client_write).await?;
                Ok(Some(bytes))
            }
            CommandAction::Reject(reason) => {
                warn!("Rejecting command: {}", reason);
                client_write.write_all(COMMAND_NOT_SUPPORTED_STATELESS).await?;
                Ok(Some(COMMAND_NOT_SUPPORTED_STATELESS.len()))
            }
            CommandAction::ForwardStateless => Ok(None),  // Caller handles forwarding
        }
    }
}
```

Then usage:
```rust
let action = CommandHandler::handle_command(&command);
if let Some(bytes) = self.handle_intercepted_command(action, &mut client_write).await? {
    backend_to_client_bytes.add(bytes);
} else {
    // Forward to backend (handler-specific logic)
    self.route_and_execute_command(...).await?;
}
```

## Other Duplication Patterns

### Byte tracking
Different types used:
- `BytesTransferred` (has `.add()` method)
- `u64` (uses `+=`)

Should standardize on one.

### Error responses
Hardcoded `COMMAND_NOT_SUPPORTED_STATELESS` everywhere. Should be in one place.

### Logging
Each handler logs slightly differently:
- per_command: `warn!("Rejecting command from client {}: {} ({})", ...)`
- standard: `warn!("Rejecting command from client {}: {}", ...)`
- cache: no logging

Should be consistent.

## Recommendation

Start with **Option 3** (simplest):
1. Extract auth + reject handling to `ClientSession::handle_intercepted_command()`
2. Each handler keeps its own ForwardStateless logic (it's genuinely different)
3. Standardize byte tracking
4. Centralize error responses and logging

This removes ~50 lines of duplication without over-abstracting.

---

# BIGGER PICTURE - Session Handler Architecture

## File Sizes
```
559 lines - src/session/handlers/per_command.rs
189 lines - src/session/handlers/hybrid.rs
109 lines - src/session/handlers/standard.rs
178 lines - src/cache/session.rs
---
1035 lines TOTAL
```

## Common Patterns Across ALL Handlers

### 1. Session Setup (DUPLICATED 4x)
```rust
// ALL handlers do this:
let (client_read, mut client_write) = client_stream.split();
let (backend_read, mut backend_write) = tokio::io::split(backend_conn);
let mut client_reader = BufReader::new(client_read);

let mut client_to_backend_bytes = 0u64;  // or BytesTransferred::zero()
let mut backend_to_client_bytes = 0u64;

let mut line = String::with_capacity(COMMAND);
```

### 2. Greeting Handling (DUPLICATED 2x)
**per_command.rs:**
```rust
if let Err(e) = client_write.write_all(PROXY_GREETING_PCR).await {
    debug!("Client {} failed to send greeting: {} (kind: {:?}). \
           This suggests the client disconnected immediately after connecting.",
        self.client_addr, e, e.kind());
    return Err(e.into());
}
backend_to_client_bytes.add(PROXY_GREETING_PCR.len());
```

**No greeting in standard/cache** - they expect backend to send it!

### 3. Command Loop (DUPLICATED 4x)
```rust
loop {
    line.clear();
    
    match client_reader.read_line(&mut line).await {
        Ok(0) => {
            debug!("Client {} disconnected", self.client_addr);
            break;
        }
        Ok(n) => { /* process command */ }
        Err(e) => { /* log error and break */ }
    }
}
```

### 4. QUIT Handling (DUPLICATED 2x)
**per_command.rs:**
```rust
if trimmed.eq_ignore_ascii_case("QUIT") {
    let _ = client_write.write_all(CONNECTION_CLOSING).await.inspect_err(|e| {
        debug!("Failed to write CONNECTION_CLOSING to client {}: {}", self.client_addr, e);
    });
    backend_to_client_bytes.add(CONNECTION_CLOSING.len());
    debug!("Client {} sent QUIT, closing connection", self.client_addr);
    break;
}
```

**standard/cache** - QUIT forwarded to backend (different behavior!)

### 5. Buffer Pool Management (PATTERN INCONSISTENCY)
- **per_command**: Uses buffer pool for backend responses
- **standard**: Uses buffer pool for bidirectional forwarding  
- **cache**: Doesn't use buffer pool at all!

### 6. Byte Tracking Inconsistency
- **per_command**: `BytesTransferred` type with `.add()` method
- **standard**: `u64` with `+=` operator
- **cache**: `u64` with `+=` operator

### 7. Error Logging (DIFFERENT APPROACHES)
- **per_command**: Custom `log_client_error()` and `log_routing_error()` functions
- **standard**: Inline `warn!()` calls
- **cache**: Minimal error logging

## Architectural Issues

### Problem 1: No Shared Session Trait
Each handler is a method on `ClientSession` but they:
- Take different parameters
- Return same type `(u64, u64)` but track bytes differently
- Have no common interface

**Could be:**
```rust
trait SessionHandler {
    async fn handle_session(&self, client: TcpStream, backend: impl AsyncRead + AsyncWrite) -> Result<TransferMetrics>;
}
```

### Problem 2: Mixed Concerns
**per_command.rs** does:
- Command parsing ✓
- Routing logic ✓
- Auth interception ✓
- Error handling ✓
- Metrics tracking ✓
- Hybrid mode switching ✓
- QUIT handling ✓

That's 7 different responsibilities!

### Problem 3: Type Inconsistency
```rust
// per_command uses:
BytesTransferred::zero()
backend_to_client_bytes.add(n)

// standard/cache use:
let mut backend_to_client_bytes = 0u64;
backend_to_client_bytes += n as u64;
```

Why have `BytesTransferred` if we don't use it everywhere?

### Problem 4: Cache Session Not Part of ClientSession
`CachingSession` is a completely separate type that DUPLICATES the entire session structure instead of wrapping/composing `ClientSession`.

Should be:
```rust
impl ClientSession {
    fn with_cache(self, cache: Arc<ArticleCache>) -> CachingClientSession { ... }
}
```

## Refactoring Opportunities (Big Picture)

### 1. Extract SessionContext
```rust
struct SessionContext {
    client_addr: SocketAddr,
    client_to_backend: BytesTransferred,
    backend_to_client: BytesTransferred,
}

impl SessionContext {
    fn log_disconnect(&self) { ... }
    fn metrics(&self) -> (u64, u64) { ... }
}
```

### 2. Extract CommandLoop
```rust
impl ClientSession {
    async fn run_command_loop<F, Fut>(
        &self,
        client_reader: &mut BufReader<impl AsyncRead>,
        mut command_handler: F,
    ) -> Result<()>
    where
        F: FnMut(&str) -> Fut,
        Fut: Future<Output = Result<LoopControl>>,
    {
        let mut line = String::with_capacity(COMMAND);
        loop {
            line.clear();
            match client_reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    match command_handler(&line).await? {
                        LoopControl::Continue => continue,
                        LoopControl::Break => break,
                    }
                }
                Err(e) => { /* log and break */ }
            }
        }
    }
}

enum LoopControl { Continue, Break }
```

### 3. Standardize on BytesTransferred
```rust
// Change standard.rs and cache/session.rs from:
let mut backend_to_client_bytes = 0u64;

// To:
let mut backend_to_client_bytes = BytesTransferred::zero();
```

### 4. Extract Session Builder Pattern
```rust
struct SessionBuilder {
    client_stream: TcpStream,
    backend_conn: Option<Box<dyn AsyncRead + AsyncWrite>>,
    with_cache: Option<Arc<ArticleCache>>,
    with_router: Option<Arc<BackendSelector>>,
}

impl SessionBuilder {
    fn build(self) -> Box<dyn SessionHandler> {
        match (self.with_cache, self.with_router) {
            (Some(cache), _) => Box::new(CachingSession { ... }),
            (None, Some(router)) => Box::new(PerCommandSession { ... }),
            (None, None) => Box::new(StandardSession { ... }),
        }
    }
}
```

### 5. Extract Greeting Handler
```rust
impl ClientSession {
    async fn send_greeting(&self, writer: &mut impl AsyncWriteExt) -> Result<usize> {
        let greeting = self.auth_handler.greeting.as_deref()
            .unwrap_or(PROXY_GREETING_PCR);
        writer.write_all(greeting.as_bytes()).await?;
        Ok(greeting.len())
    }
}
```

## Recommended Refactoring Order

### Phase 1: Unify Metrics (Easy Win)
1. Change standard.rs to use `BytesTransferred`
2. Change cache/session.rs to use `BytesTransferred`
3. Remove all `as u64` casts

**Impact**: ~30 lines, makes code consistent

### Phase 2: Extract Command Handling (What we started)
1. Extract `handle_intercepted_command()` method
2. Update all 4 handlers to use it

**Impact**: ~50 lines removed

### Phase 3: Extract Session Context
1. Create `SessionContext` struct
2. Thread it through all handlers
3. Centralize logging

**Impact**: ~40 lines removed, better logging

### Phase 4: Extract Command Loop (Hard)
1. Create generic `run_command_loop()` method
2. Migrate per_command to use it
3. Migrate standard to use it  
4. Migrate cache to use it

**Impact**: ~150 lines removed, but risky

### Phase 5: Unify Cache Session (Medium)
1. Make `CachingSession` wrap `ClientSession` instead of duplicating
2. Use composition not duplication

**Impact**: ~80 lines removed

## Total Potential Savings
- Phase 1: 30 lines
- Phase 2: 50 lines
- Phase 3: 40 lines
- Phase 4: 150 lines
- Phase 5: 80 lines
**TOTAL: ~350 lines removed from 1035 = 33% reduction**

Plus: More consistent, easier to maintain, easier to add auth.
