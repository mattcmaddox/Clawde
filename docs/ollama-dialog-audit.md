# Audit: Ollama Config Dialog Feature

## Critical Bugs (P0)

### 1. Async ping is not implemented
**Location**: `app.rs:5869-5872`
**Impact**: Health dot is always "Untested", model picker can never be populated from server

The ping is stubbed with `// TODO: Spawn async ping task`. Without this:
- Health dot never changes from gray
- User can never reach the model picker (SelectModel phase)
- No server connectivity verification

**Fix**: Implement `tokio::spawn` that sends `QueryEvent::OllamaPingResult` back to the TUI event loop.

### 2. Model picker unreachable in Default view
**Location**: `app.rs:5858-5888`
**Impact**: User cannot select a model from the server

In Default phase, there's no way to reach the model picker. The only way to get to `SelectModel` is through `ping_success()`, which requires the async ping to be implemented.

**Fix**: Once async ping is implemented, add a keybinding (e.g., `m`) in Default view to trigger ping + model picker.

### 3. `activate_provider` opens model picker after Ollama dialog
**Location**: `app.rs:5868-5876` + `app.rs:2670`
**Impact**: Redundant model picker opens after user already selected model

When Enter is pressed in Default view:
1. `take_values()` returns (host, model)
2. `persist_ollama_config()` saves the model
3. `activate_provider()` calls `open_model_picker_for_provider()` which opens the model picker again

This is confusing — user just picked a model, now they see another picker.

**Fix**: Either skip `activate_provider` for Ollama (use a custom activation path) or set the model before opening the picker so it's pre-selected.

## Medium Issues (P1)

### 4. No URL validation
**Location**: `ollama_config_dialog.rs:179`
**Impact**: User can connect with invalid URLs

`can_connect()` only checks `!host_url_input.trim().is_empty()`. No validation for:
- Valid URL scheme (http/https)
- Valid hostname/IP
- Valid port range

**Fix**: Add basic URL validation before allowing connection.

### 5. No model name validation
**Location**: `app.rs:5868`
**Impact**: User can save invalid model names

When connecting from Default view, the model name is saved without checking if it exists on the server.

**Fix**: Either validate against the server or at minimum check for empty/whitespace.

### 6. Persist function doesn't handle errors
**Location**: `app.rs:2721-2738`
**Impact**: Config may not be saved

`persist_ollama_config` uses `let _ = settings.save_sync()` which silently drops errors.

**Fix**: Handle the error and show a status message.

### 7. Health dot doesn't auto-refresh
**Location**: `ollama_config_dialog.rs:145-147`
**Impact**: Health status is stale after edits

Health is set on `open()` to `Untested` and only updates on `ping_success()`/`ping_failed()`. If user edits the host URL, the health dot doesn't update.

**Fix**: Reset health to `Untested` when host URL is edited in edit mode.

### 8. No way to go back from model picker to edit
**Location**: `app.rs:5948-5975`
**Impact**: User must close dialog and reopen to change host

In `SelectModel` phase, Esc closes the entire dialog. There's no way to go back to edit the host URL.

**Fix**: Add a "back" action (e.g., Esc or Backspace) that returns to Default view instead of closing.

## Minor Issues (P2)

### 9. No vim j/k in Default view when not in vim mode
**Location**: `app.rs:5878-5882`
**Impact**: j/k always navigate in Default view, even without vim mode

The Default view always responds to j/k for navigation, but the hint text doesn't show this unless vim is enabled. This is inconsistent.

**Fix**: Always show j/k in hints, or only respond to j/k when vim is enabled.

### 10. Edit mode cursor position not tracked
**Location**: `ollama_config_dialog.rs:105-112`
**Impact**: Cursor always appears at end of text

`insert_char` always appends to the end. There's no cursor position tracking, so user can't insert text in the middle.

**Fix**: Add cursor position tracking (like `PromptInputState`).

### 11. No keyboard shortcut to trigger ping from edit mode
**Location**: `app.rs:5890-5932`
**Impact**: User must exit edit mode to trigger ping

In edit mode, there's no way to ping the server without first confirming the edit (Enter) then pressing Enter again in Default view.

**Fix**: Add a shortcut (e.g., Ctrl+P) to trigger ping from edit mode.

### 12. Model picker doesn't show parameter size
**Location**: `ollama_config_dialog.rs:673-680`
**Impact**: User can't see model parameter count

The model picker shows name, size, and quantization but not parameter size (e.g., "7B", "13B").

**Fix**: Add parameter_size column to the model picker.

### 13. No timeout on ping
**Location**: N/A (ping not implemented)
**Impact**: Ping could hang indefinitely

When async ping is implemented, there's no timeout. A slow/unresponsive server could block the dialog.

**Fix**: Add a timeout (e.g., 5 seconds) to the ping request.

## Summary

| Severity | Count | Key Issue |
|---|---|---|
| P0 (Critical) | 3 | Async ping not implemented, model picker unreachable, redundant picker |
| P1 (Medium) | 5 | No URL/model validation, persist errors, health stale, no back navigation |
| P2 (Minor) | 5 | Vim inconsistency, cursor tracking, ping shortcut, model params, timeout |

## Recommended Fix Order

1. **Implement async ping** (fixes P0 #1, #2, #7)
2. **Fix activate_provider for Ollama** (fixes P0 #3)
3. **Add URL validation** (fixes P1 #4)
4. **Add back navigation from model picker** (fixes P1 #8)
5. **Handle persist errors** (fixes P1 #6)
6. **Add ping timeout** (fixes P2 #13)

## Test Coverage

Current tests cover:
- Open/close
- Field navigation
- Edit mode
- Backspace
- can_connect
- Model navigation
- take_values
- Model size display
- Health status
- is_modal
- retry_from_failure

Missing test coverage:
- URL validation
- Model validation
- Persist error handling
- Health reset on edit
- Back navigation from model picker
- Ping timeout
