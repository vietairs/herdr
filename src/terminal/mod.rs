mod id;
mod runtime;
mod runtime_registry;
mod source;
pub mod state;
mod title;

pub use id::TerminalId;
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub(crate) use source::{LocalChild, TerminalSource};
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
