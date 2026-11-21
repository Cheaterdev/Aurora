//! Prompt templates: the first message a run starts with.
//!
//! * [`lang`] is the template language: parsing, validation, rendering. Pure.
//! * [`catalogue`] lists the variables a template may use.
//! * [`builtin`] holds the default template.
//! * [`context`] gathers the values for one issue (read-only, bounded).
//! * [`templates`] stores overrides, resolves which template applies and
//!   renders; [`render_prompt`] is the one function the runner calls.
//!
//! # Which template applies
//!
//! The most specific wins: the agent's override for the issue's project and
//! issue type, then its override for the project, then the agent's own
//! `instructions` (if not blank), then the built-in default. Overrides are
//! rows of `prompt_template`; the agent default is not copied there.
//!
//! # Events
//!
//! `prompt_template.saved` and `prompt_template.deleted`, entity kind `agent`
//! with the agent's id, no project on the event, written in the transaction of
//! the change. They never carry the template text, only its length; see
//! [`templates`].

pub mod builtin;
pub mod catalogue;
pub mod context;
pub mod lang;
pub mod templates;

pub use builtin::DEFAULT_TEMPLATE;
pub use catalogue::{VARIABLES, Variable, VariableInfo, catalogue as variable_catalogue};
pub use context::{
    Context, MAX_COMMENT_CHARS, MAX_COMMENTS, MAX_LIST_ITEMS, PromptRequest, TriggerCtx,
    TriggerKind, context_for, iso8601,
};
pub use lang::{
    MAX_DEPTH, MAX_OUTPUT_CHARS, MAX_TEMPLATE_CHARS, TRUNCATED_SUFFIX, Template, TemplateError,
    parse, render,
};
pub use templates::{
    PromptTemplate, Rendered, ResolvedTemplate, SaveOutcome, TemplateSource, agent_default_source,
    check_template, delete_template, list_templates, render_prompt, resolve_template,
    save_template,
};
