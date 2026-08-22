# Plan: Ollama TUI Configuration Dialog

## Existing Patterns (Analyzed)

| Pattern | File | Fields | Use Case |
|---|---|---|---|
| `DialogSelectState` | `dialog_select.rs` | Filterable list | Provider picker (/connect) |
| `CustomProviderDialogState` | `custom_provider_dialog.rs` | URL + API key | Custom OpenAI-compatible |
| `KeyInputDialogState` | `key_input_dialog.rs` | Single masked field | API key entry |
| `FreeModeDialogState` | `free_mode_dialog.rs` | Multi-provider keys | Free tier setup |
| `SettingsScreen` | `settings_screen.rs` | Searchable flat list | All settings |

## Recommended Pattern: `CustomProviderDialog` + Model Picker

The Ollama config dialog combines:
1. **CustomProviderDialog-style** text input for the host URL
2. **Model picker** (like the existing model picker) for selecting a model

## Proposed Dialog: `OllamaConfigDialog`

### Fields

| Field | Type | Default | Description |
|---|---|---|---|
| **Host URL** | Text input | `http://192.168.1.45:11434` | Ollama server address |
| **Model** | Text input + picker | `qwen2.5-coder:3b` | Model to use |

### Layout

**Phase 1: Host URL Input**

```
┌─ Connect Ollama ────────────────────────────────── esc ┐
│                                                        │
│  Host URL:                                             │
│  http://192.168.1.45:11434_                            │
│                                                        │
│  tab switch field  enter confirm                       │
└────────────────────────────────────────────────────────┘
```

**Phase 2: Model Selection (after ping succeeds)**

```
┌─ Select Model ──────────────────────────────────── esc ┐
│                                                        │
│  Available models on 192.168.1.45:                     │
│                                                        │
│  ▸ qwen2.5-coder:3b                    1.8GB  Q4_K_M  │
│    qwen2.5-coder:7b                    4.4GB  Q4_K_M  │
│    deepseek-coder:latest               0.8GB  Q4_0    │
│    deepseek-r1:1.5b                    1.1GB  Q4_K_M  │
│                                                        │
│  j/k select  enter confirm  esc back                   │
└────────────────────────────────────────────────────────┘
```

### State Structure

```rust
pub enum OllamaConfigField {
    HostUrl,
    Model,
}

pub enum OllamaConfigPhase {
    /// User is entering the host URL
    EnterHost,
    /// Pinging the server to verify connectivity
    Pinging,
    /// Ping failed, showing error
    PingFailed(String),
    /// Ping succeeded, showing model list
    SelectModel,
}

pub struct OllamaConfigDialogState {
    pub visible: bool,
    pub last_rect: Cell<Rect>,
    pub host_url_input: String,
    pub model_input: String,
    pub active_field: OllamaConfigField,
    pub phase: OllamaConfigPhase,
    pub models: Vec<OllamaModel>,
    pub selected_model_idx: usize,
    pub vim_search: VimSearch,
}

pub struct OllamaModel {
    pub name: String,
    pub size: u64,
    pub quantization: String,
    pub parameter_size: String,
}
```

### Methods

```rust
impl OllamaConfigDialogState {
    pub fn new() -> Self;
    pub fn open(&mut self, current_url: Option<String>, current_model: Option<String>);
    pub fn close(&mut self);
    pub fn move_next_field(&mut self);
    pub fn move_prev_field(&mut self);
    pub fn insert_char(&mut self, c: char);
    pub fn backspace(&mut self);
    pub fn can_submit(&self) -> bool;

    // Phase transitions
    pub fn start_ping(&mut self);
    pub fn ping_success(&mut self, models: Vec<OllamaModel>);
    pub fn ping_failed(&mut self, error: String);

    // Model selection
    pub fn move_model_up(&mut self);
    pub fn move_model_down(&mut self);
    pub fn selected_model(&self) -> Option<&OllamaModel>;

    // Final values
    pub fn take_values(&mut self) -> (String, String);  // (host_url, model)
}
```

### Render Functions

```rust
pub fn render_ollama_config_dialog(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    vim_enabled: bool,
    area: Rect,
);

fn render_host_input(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    vim_enabled: bool,
    area: Rect,
);

fn render_pinging(frame: &mut Frame, area: Rect);

fn render_ping_failed(
    frame: &mut Frame,
    error: &str,
    area: Rect,
);

fn render_model_picker(
    frame: &mut Frame,
    state: &OllamaConfigDialogState,
    area: Rect,
);
```

### Integration Points

#### 1. Wire into `App` struct (`app.rs`)

```rust
// Add field
pub ollama_config_dialog: OllamaConfigDialogState,

// In new()
ollama_config_dialog: OllamaConfigDialogState::new(),

// In close_all_modals()
self.ollama_config_dialog.close();
```

#### 2. Wire into connect flow (`app.rs` ~line 5960)

When Ollama is selected in the connect dialog:

```rust
"ollama" => {
    // Load current config
    let current_url = Settings::load_sync().ok().and_then(|s| {
        s.config.provider_configs
            .get("ollama")
            .and_then(|c| c.api_base.clone())
    });
    let current_model = Settings::load_sync().ok().and_then(|s| {
        s.config.provider_configs
            .get("ollama")
            .and_then(|c| c.options.get("model"))
            .and_then(|v| v.as_str())
            .map(String::from)
    });
    self.ollama_config_dialog.open(current_url, current_model);
}
```

#### 3. Wire into key handler (`app.rs` ~line 5557)

```rust
if self.ollama_config_dialog.visible {
    match self.ollama_config_dialog.phase {
        OllamaConfigPhase::EnterHost => {
            // Handle text input for host URL
            match self.ollama_config_dialog.vim_search.handle_key(...) {
                VimSearchKey::Consumed => return false,
                VimSearchKey::PushChar(c) => {
                    self.ollama_config_dialog.insert_char(c);
                    return false;
                }
                VimSearchKey::PopChar => {
                    self.ollama_config_dialog.backspace();
                    return false;
                }
                VimSearchKey::Passthrough => {}
            }
            match key.code {
                KeyCode::Esc => self.ollama_config_dialog.close(),
                KeyCode::Enter => {
                    // Start ping
                    self.ollama_config_dialog.start_ping();
                    // Spawn async ping task
                    let url = self.ollama_config_dialog.host_url_input.clone();
                    let tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        match ping_ollama_server(&url).await {
                            Ok(models) => {
                                let _ = tx.send(QueryEvent::OllamaPingResult(Ok(models)));
                            }
                            Err(e) => {
                                let _ = tx.send(QueryEvent::OllamaPingResult(Err(e)));
                            }
                        }
                    });
                }
                KeyCode::Backspace if !vim_enabled => self.ollama_config_dialog.backspace(),
                KeyCode::Char(c) if !vim_enabled => self.ollama_config_dialog.insert_char(c),
                _ => {}
            }
        }
        OllamaConfigPhase::SelectModel => {
            // Handle model selection
            match key.code {
                KeyCode::Esc => self.ollama_config_dialog.close(),
                KeyCode::Up | KeyCode::Char('k') if vim_enabled => {
                    self.ollama_config_dialog.move_model_up();
                }
                KeyCode::Down | KeyCode::Char('j') if vim_enabled => {
                    self.ollama_config_dialog.move_model_down();
                }
                KeyCode::Enter => {
                    if let Some(model) = self.ollama_config_dialog.selected_model() {
                        let model_name = model.name.clone();
                        let (host_url, _) = self.ollama_config_dialog.take_values();
                        self.persist_ollama_config(&host_url, &model_name);
                        self.activate_provider("ollama".to_string(), "Ollama".to_string(), "Connected to");
                    }
                }
                _ => {}
            }
        }
        _ => {}  // Pinging, PingFailed — no input
    }
    return false;
}
```

#### 4. Handle ping result (`app.rs`)

```rust
QueryEvent::OllamaPingResult(result) => {
    match result {
        Ok(models) => self.ollama_config_dialog.ping_success(models),
        Err(e) => self.ollama_config_dialog.ping_failed(e),
    }
}
```

#### 5. Wire into render (`render.rs` ~line 970)

```rust
if app.ollama_config_dialog.visible {
    render_ollama_config_dialog(
        frame,
        &app.ollama_config_dialog,
        app.prompt_input.vim_enabled,
        size,
    );
}
```

#### 6. Persist config (`app.rs`)

```rust
fn persist_ollama_config(&mut self, host_url: &str, model: &str) {
    let mut settings = Settings::load_sync().unwrap_or_default();

    // Normalize the host URL (strip /v1 if present)
    let normalized_host = clawde_core::config::normalize_ollama_host(host_url)
        .unwrap_or_else(|| host_url.to_string());

    let provider = settings.config.provider_configs
        .entry("ollama".to_string())
        .or_default();

    provider.api_base = Some(format!("{}/v1", normalized_host));
    provider.options.insert(
        "default_host".to_string(),
        serde_json::json!(normalized_host),
    );
    provider.options.insert(
        "model".to_string(),
        serde_json::json!(model),
    );

    let _ = settings.save_sync();
    self.auth_store.reload();
}
```

### Async Ping Function

```rust
async fn ping_ollama_server(url: &str) -> Result<Vec<OllamaModel>, String> {
    let normalized = clawde_core::config::normalize_ollama_host(url)
        .map_err(|e| format!("Invalid URL: {}", e))?;

    let tags_url = format!("{}/api/tags", normalized);
    let response = reqwest::get(&tags_url)
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Server returned status: {}", response.status()));
    }

    let data: serde_json::Value = response.json()
        .await
        .map_err(|e| format!("Invalid response: {}", e))?;

    let models = data["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    Some(OllamaModel {
                        name: m["name"].as_str()?.to_string(),
                        size: m["size"].as_u64().unwrap_or(0),
                        quantization: m["details"]["quantization_level"]
                            .as_str()
                            .unwrap_or("?")
                            .to_string(),
                        parameter_size: m["details"]["parameter_size"]
                            .as_str()
                            .unwrap_or("?")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(models)
}
```

### File Changes

| File | Change |
|---|---|
| `src-rust/crates/tui/src/ollama_config_dialog.rs` | **NEW** — Dialog state + render |
| `src-rust/crates/tui/src/lib.rs` | Add `pub mod ollama_config_dialog;` + re-exports |
| `src-rust/crates/tui/src/app.rs` | Add field, wire open/close/handle_key + ping result |
| `src-rust/crates/tui/src/render.rs` | Add render call |

### Testing

1. **Unit tests** in `ollama_config_dialog.rs`:
   - `test_open_close`
   - `test_field_navigation`
   - `test_char_insertion`
   - `test_backspace`
   - `test_can_submit`
   - `test_model_navigation`
   - `test_take_values`

2. **Integration test** in `app.rs`:
   - `test_ollama_config_dialog_opens_from_connect`
   - `test_ollama_config_dialog_persists_config`
   - `test_ollama_config_dialog_model_selection`

### Future Enhancements

1. **Custom model input** — Allow typing a model name not in the list
2. **Model info** — Show VRAM requirements and whether it fits
3. **Connection test** — Try a simple inference call before saving
4. **Model pull** — Option to pull a model directly from the dialog
