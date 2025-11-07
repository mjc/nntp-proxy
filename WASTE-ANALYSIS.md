# Code Waste Analysis - 1KLOC+ of Duplication

**Total codebase**: 15,415 lines  
**Estimated waste**: 1,200-1,500 lines (8-10%)

---

## 1. Command Classifier (1056 lines) - ~400 lines wasted

**File**: `src/command/classifier.rs`

### Breakdown:
- 448 lines of tests (42%)
- 608 lines of code

### The Waste:
**23 command definitions** using macro, each with 3 case variants:

```rust
command_cases!(
    ARTICLE_CASES,
    "ARTICLE",
    "article",
    "Article",
    "[RFC 3977 §6.2.1]... - ARTICLE command\nRetrieve article..."
);
```

This pattern repeats 23 times for:
- ARTICLE, BODY, HEAD, STAT (message retrieval)
- GROUP, NEXT, LAST, LISTGROUP (navigation)
- LIST, DATE, CAPABILITIES, MODE, HELP, QUIT (stateless)
- XOVER, OVER, XHDR, HDR (headers)
- POST, IHAVE, NEWGROUPS, NEWNEWS (posting)
- AUTHINFO (auth)

### Current Matching Code:
```rust
if matches_any(cmd, ARTICLE_CASES)
    || matches_any(cmd, BODY_CASES)
    || matches_any(cmd, HEAD_CASES)
    || matches_any(cmd, STAT_CASES)
{
    return Self::Stateful;
}

if matches_any(cmd, GROUP_CASES) {
    return Self::Stateful;
}

if matches_any(cmd, AUTHINFO_CASES) {
    return Self::classify_authinfo(bytes, cmd_end);
}

if matches_any(cmd, LIST_CASES)
    || matches_any(cmd, DATE_CASES)
    || matches_any(cmd, CAPABILITIES_CASES)
    || matches_any(cmd, MODE_CASES)
    || matches_any(cmd, HELP_CASES)
    || matches_any(cmd, QUIT_CASES)
{
    return Self::Stateless;
}

// ... continues for all 23 commands
```

### Solution: Use phf (perfect hash) or match statement
```rust
use phf::phf_map;

static COMMANDS: phf::Map<&'static str, NntpCommand> = phf_map! {
    "ARTICLE" => NntpCommand::Stateful,
    "article" => NntpCommand::Stateful,
    "Article" => NntpCommand::Stateful,
    "BODY" => NntpCommand::Stateful,
    // ... etc
};

// Or even simpler with case-insensitive match:
match cmd.to_ascii_uppercase().as_bytes() {
    b"ARTICLE" | b"BODY" | b"HEAD" | b"STAT" => Self::Stateful,
    b"GROUP" => Self::Stateful,
    b"LIST" | b"DATE" | b"CAPABILITIES" | b"MODE" | b"HELP" | b"QUIT" => Self::Stateless,
    // ...
    _ => Self::Stateless,
}
```

**Lines saved**: ~300 lines of boilerplate command definitions

**Performance note**: The "zero allocation" claim is misleading - the current code does `memchr` on every command anyway. A simple `to_ascii_uppercase()` would be ~5ns and WAY simpler.

---

## 2. Validated Types (482 lines) - ~400 lines wasted

**File**: `src/types/validated.rs`

### The Waste:
Only **2 types** (HostName, ServerName) but 482 lines!
- 314 lines of tests (65%)
- 168 lines of code for 2 nearly-identical types

### Each Type Has Identical Pattern:
```rust
pub struct HostName(String);

impl HostName {
    pub fn new(s: String) -> Result<Self, ValidationError> { /* validate */ }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl AsRef<str> for HostName { /* ... */ }
impl Deref for HostName { /* ... */ }
impl Display for HostName { /* ... */ }
impl TryFrom<String> for HostName { /* ... */ }
impl Deserialize for HostName { /* ... */ }
```

**EVERY SINGLE IMPL IS IDENTICAL** between HostName and ServerName!

### Solution: Macro or derive
```rust
macro_rules! validated_string {
    ($name:ident, $error:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        
        impl $name {
            pub fn new(s: String) -> Result<Self, ValidationError> {
                if s.trim().is_empty() {
                    Err(ValidationError::$error)
                } else {
                    Ok(Self(s))
                }
            }
            // ... all the other impls
        }
    }
}

validated_string!(HostName, EmptyHostName, "A validated hostname");
validated_string!(ServerName, EmptyServerName, "A validated server name");
```

**Lines saved**: ~350 lines (keep ~130 for macro + tests)

---

## 3. Session Handlers (1035 lines) - ~350 lines wasted

**Files**:
- `src/session/handlers/per_command.rs` (559 lines)
- `src/session/handlers/standard.rs` (109 lines)
- `src/session/handlers/hybrid.rs` (189 lines)
- `src/cache/session.rs` (178 lines)

### Duplication Already Identified:
See DUPLICATION-ANALYSIS.md for full details.

**Summary**:
- Command loop: duplicated 4x
- Session setup: duplicated 4x
- Command action matching: duplicated 4x
- Byte tracking (inconsistent types): 4x
- Error handling: 4x with different approaches
- QUIT handling: 2x
- Buffer management: 3 different approaches

**Lines saved**: ~350 lines

---

## 4. Test Code Inline vs Separate Files

### Current State:
- `src/command/classifier.rs`: 448 lines of tests inline (42% of file)
- `src/types/validated.rs`: 314 lines of tests inline (65% of file)
- `src/types/metrics.rs`: 230 lines of tests inline (61% of file)
- `src/types/protocol.rs`: 165 lines of tests inline (38% of file)

### Already Good:
- ✅ `src/protocol/response.rs`: Tests in `response/tests/` (508 lines separate)

### Problem:
Inline tests make files harder to navigate and slow down compilation (changing test code recompiles the entire module).

### Solution:
Move tests to separate `tests/` subdirectories like `response.rs` does.

**Lines saved**: 0 (just reorganization, but better DX)

---

## 5. Type Duplication - BytesTransferred vs u64

### Found in Session Handlers:
```rust
// per_command.rs uses:
let mut client_to_backend_bytes = BytesTransferred::zero();
backend_to_client_bytes.add(n);

// standard.rs uses:
let mut client_to_backend_bytes = 0u64;
backend_to_client_bytes += n as u64;

// cache/session.rs uses:
let mut client_to_backend_bytes = 0u64;
client_to_backend_bytes += n as u64;
```

### Why Do We Even Have BytesTransferred?
Looking at `src/types/protocol.rs` (434 lines):

```rust
pub struct BytesTransferred {
    bytes: u64,
}

impl BytesTransferred {
    pub fn zero() -> Self { Self { bytes: 0 } }
    pub fn add(&mut self, n: usize) { self.bytes += n as u64; }
    pub fn as_u64(&self) -> u64 { self.bytes }
}
```

It's just a wrapper around `u64` with an `add()` method!

### Problem:
- 3 files use `u64` directly
- 1 file uses `BytesTransferred`
- The wrapper provides no value (could just use `u64`)
- OR if we want the wrapper, use it everywhere

**Lines saved**: ~50 lines (delete `BytesTransferred` type, use `u64` everywhere)

---

## 6. Connection Pool Abstraction Missing

**File**: `src/pool/provider.rs` (530 lines)

### Current State:
Tightly coupled to `deadpool`:
```rust
pub struct DeadpoolConnectionProvider {
    pool: deadpool::managed::Pool<NntpConnectionManager>,
    // ...
}
```

### Problem:
- Can't test without real TCP connections
- Can't mock easily
- Hard to swap pool implementations

### Already Documented:
See REFACTORING-OPPORTUNITIES.md #3 - "Create Connection Pool Abstraction Trait"

**Lines saved**: 0 (adds abstraction but enables better testing)

---

## 7. Config Types Duplication

**File**: `src/config/types.rs` (385 lines)  
**File**: `src/types/config/mod.rs` (407 lines)

Wait, why are there TWO config modules?

Let me check...

Actually `src/types/config/` seems to be re-exports. Let me verify...

**Lines saved**: TBD (need to investigate)

---

## SUMMARY - Top Opportunities

| File | Total Lines | Wasted | % | Fix |
|------|------------|--------|---|-----|
| `command/classifier.rs` | 1056 | ~400 | 38% | Use match or phf instead of macro boilerplate |
| `types/validated.rs` | 482 | ~350 | 73% | Macro for validated types |
| Session handlers | 1035 | ~350 | 34% | Extract common patterns |
| `types/protocol.rs` (BytesTransferred) | 434 | ~50 | 12% | Delete wrapper, use u64 |
| Test organization | 1157 | 0 | 0% | Move to separate files |

**TOTAL IMMEDIATE WASTE: ~1,150 lines** that could be removed with refactoring.

---

## Refactoring Priority

### Phase 1: Quick Wins (1-2 days)
1. **Delete BytesTransferred, use u64** everywhere
   - Saves: ~50 lines
   - Risk: Low (simple find/replace)
   - Impact: Code consistency

2. **Macro for validated types**
   - Saves: ~350 lines
   - Risk: Low (already suggested in REFACTORING-OPPORTUNITIES.md)
   - Impact: Easy to add new validated types

### Phase 2: Medium Effort (3-5 days)
3. **Extract session handler patterns**
   - Saves: ~350 lines
   - Risk: Medium (needs careful testing)
   - Impact: Makes auth refactoring much easier

4. **Move inline tests to separate files**
   - Saves: 0 lines, better DX
   - Risk: Low (mechanical move)
   - Impact: Faster compile times, easier navigation

### Phase 3: Bigger Refactor (1-2 weeks)
5. **Simplify command classifier**
   - Saves: ~400 lines
   - Risk: High (hot path, performance critical)
   - Impact: Simpler code, but need benchmarks

---

## The Real Win

After these refactorings, adding config file auth becomes:
1. ✅ Add username/password to config (already done)
2. ✅ Make AuthHandler stateful (already done)
3. ✅ Add `handle_auth_command()` in ONE place (already done)
4. Change session handlers to call it (EASY because we extracted the pattern)
5. Thread AuthHandler through constructors (EASY)

**Estimated time to add auth AFTER refactoring**: 2-3 hours  
**Estimated time to add auth WITHOUT refactoring**: 1-2 days (changing 4 different files, lots of testing)

The refactoring pays for itself on the first feature addition!
