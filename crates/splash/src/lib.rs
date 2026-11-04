//! Splash's server layer in Rust, with no Python: Heimdall's own front for the Splash native engine.
//!
//! The native engine (`splash serve-native`, Apache-2.0, bundled with its notices) is a child process that speaks a
//! binary framed protocol on stdin/stdout (`protocol`, `engine`). Everything the Python server did around it that
//! Heimdall uses is here: the model's chat template and tokenizer (`chat`), constrained output for tool calls and JSON
//! schemas through llguidance (`constrain`), model download and preparation from Hugging Face (`hub`), and the
//! OpenAI-compatible HTTP API Heimdall's agent and background calls speak (`server`).
//! The Python reference is Splash 1.4.1 (`/opt/homebrew/Cellar/splash/1.4.1/libexec`); each module names the files it
//! ports. `docs/splash-native.md` is the map.
pub mod chat;
pub mod constrain;
pub mod engine;
pub mod hub;
pub mod images;
pub mod protocol;
pub mod server;
