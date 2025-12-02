# RFC 3977 NNTP Response Codes

Complete reference from [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1) and [RFC 4643 (Authentication)](https://datatracker.ietf.org/doc/html/rfc4643).

## 1xx - Informational (All Multiline)

- **100** - Help text follows
- **101** - Capabilities list follows
- **111** - Date (DATE command response)

## 2xx - Success (Selected Multiline)

### Connection/Session
- **200** - Server ready, posting allowed (Greeting)
- **201** - Server ready, posting NOT allowed (Greeting)
- **202** - Slave status noted (SLAVE)
- **203** - Streaming permitted (MODE STREAM)
- **205** - Connection closing (QUIT)
- **211** - Group selected

### Article Operations (Single Line)
- **220** - Article follows (head and body) **[MULTILINE]**
- **221** - Head follows **[MULTILINE]**
- **222** - Body follows **[MULTILINE]**
- **223** - Article exists (no text follows)
- **224** - Overview information follows **[MULTILINE]**

### Posting
- **225** - Headers follow **[MULTILINE]**
- **230** - List of new articles follows **[MULTILINE]**
- **231** - List of new newsgroups follows **[MULTILINE]**
- **235** - Article transferred OK (IHAVE)
- **238** - Article not wanted (CHECK - streaming)
- **239** - Article transferred OK (TAKETHIS - streaming)
- **240** - Article posted

### Listing
- **215** - Information follows **[MULTILINE]**

## 3xx - Intermediate Success (Require More Input)

- **335** - Send article to be transferred (IHAVE)
- **340** - Send article to be posted (POST)
- **350** - Continue sending article (CHECK - streaming)

### Authentication (RFC 4643)
- **381** - Password required (AUTHINFO PASS)
- **383** - Account/OTP required (not commonly implemented)

## 4xx - Temporary Errors

### Article Errors
- **400** - Service discontinued
- **403** - Internal fault/command fault (generic error)
- **411** - No such newsgroup
- **412** - No newsgroup selected
- **420** - Current article number is invalid
- **421** - No next article in this group
- **422** - No previous article in this group
- **423** - No article with that number
- **430** - No article with that message-ID
- **435** - Article not wanted (IHAVE)
- **436** - Transfer not possible, try again later (IHAVE)
- **437** - Transfer rejected, do not retry (IHAVE)
- **438** - Article not wanted (CHECK - streaming)
- **439** - Transfer rejected, do not retry (TAKETHIS - streaming)
- **440** - Posting not permitted
- **441** - Posting failed

### Resource/State Errors
- **450** - Authorization required (MODE READER)
- **451** - Internal error/timeout
- **452** - Article not filed, try again

## 5xx - Permanent Errors

### Command/Protocol Errors
- **500** - Unknown/unsupported command
- **501** - Command syntax error
- **502** - Service permanently unavailable
- **503** - Feature not supported

### Authentication (RFC 4643)
- **480** - Authentication required for command
- **481** - Authentication rejected (AUTHINFO USER)
- **482** - Authentication rejected (AUTHINFO PASS)
- **483** - Encryption required (not commonly implemented)

## Response Code Categories

### Multiline Responses
All responses that send multiline data (terminated by `.\r\n`):
- All 1xx codes (100-199)
- Selected 2xx codes: 215, 220, 221, 222, 224, 225, 230, 231, 282

### Success Codes (Non-Error)
- 2xx: Command successful
- 3xx: Command successful so far, more input needed

### Error Codes
- 4xx: Temporary failure (client may retry)
- 5xx: Permanent failure (client should not retry)

## Special Handling

### Greetings (Initial Connection)
- 200: Posting allowed
- 201: No posting allowed

### Disconnect
- 205: Connection closing (response to QUIT)

### Authentication Flow (RFC 4643)
1. **480** → AUTHINFO USER → **381** → AUTHINFO PASS → **281** (success) or **482** (fail)
2. **480** → AUTHINFO USER → **481** (reject username)

### Streaming (RFC 4644)
- **203**: MODE STREAM accepted
- **238**: Article not wanted (CHECK)
- **350**: Send article (CHECK - go ahead)
- **438**: Article not wanted (CHECK - streaming)
- **239**: Article transferred OK (TAKETHIS)
- **439**: Article rejected (TAKETHIS)
