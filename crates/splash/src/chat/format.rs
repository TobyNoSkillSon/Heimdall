//! The chat format of each model family (server/chat_format.py, install/families.py): how its model frames reasoning
//! and lays out tool calls, and the token ids it shares with the engine.

pub const THINK_START: &str = "<think>";
pub const THINK_END: &str = "</think>";
pub const TOOL_CALL_OPEN: &str = "<tool_call>";
pub const TOOL_CALL_CLOSE: &str = "</tool_call>";

/// How a family's template lays a tool call out between `<tool_call>` and `</tool_call>` (chat_format.ToolCallSyntax).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCallSyntax {
    pub name_open: &'static str,
    pub name_close: &'static str,
    pub key_open: &'static str,
    pub key_close: &'static str,
    pub value_open: &'static str,
    pub value_close: &'static str,
    pub arguments_close: &'static str,
    pub tags_are_tokens: bool,
}

impl ToolCallSyntax {
    /// The text a call opens with, up to its function's name.
    pub fn call_open(&self) -> String {
        format!("{TOOL_CALL_OPEN}{}", self.name_open)
    }
    pub fn function_opening(&self, name: &str) -> String {
        format!("{}{name}{}", self.name_open, self.name_close)
    }
    /// The character a function's name ends at.
    pub fn name_end(&self) -> char {
        let tag = if self.name_close.is_empty() { self.key_open } else { self.name_close };
        tag.chars().next().expect("a call tag")
    }
    /// The tags that may follow an argument.
    pub fn argument_tags(&self) -> Vec<&'static str> {
        [self.key_open, self.arguments_close.trim(), TOOL_CALL_CLOSE]
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect()
    }
    /// Where a value ends: at its close followed by a tag that may follow an argument.
    pub fn value_ends(&self) -> Vec<String> {
        self.argument_tags().iter().map(|t| format!("{}{t}", self.value_close)).collect()
    }
}

pub const QWEN_XML: ToolCallSyntax = ToolCallSyntax {
    name_open: "\n<function=",
    name_close: ">\n",
    key_open: "<parameter=",
    key_close: ">",
    value_open: "\n",
    value_close: "\n</parameter>\n",
    arguments_close: "</function>\n",
    tags_are_tokens: false,
};

pub const GLM_ARGS: ToolCallSyntax = ToolCallSyntax {
    name_open: "",
    name_close: "",
    key_open: "<arg_key>",
    key_close: "</arg_key>",
    value_open: "<arg_value>",
    value_close: "</arg_value>",
    arguments_close: "",
    tags_are_tokens: true,
};

/// What the server reads, writes and checks in a family's tokens (chat_format.ChatFormat).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatFormat {
    /// The model's vocabulary: the rows each constraint mask covers.
    pub vocabulary: u32,
    /// The engine's stop tokens with their text; the tokenizer's EOS is one.
    pub stop_tokens: &'static [(&'static str, u32)],
    pub think_end_id: u32,
    /// Where the model opens its reasoning itself (Flint); None where the generation prompt opens it (Qwen).
    pub think_start_id: Option<u32>,
    pub call_open_id: u32,
    pub call_syntax: ToolCallSyntax,
    /// The special token after which prompt tokenization may reuse a cached prompt's tokens.
    pub message_boundary: &'static str,
}

impl ChatFormat {
    pub fn stop_ids(&self) -> Vec<u32> {
        self.stop_tokens.iter().map(|(_, id)| *id).collect()
    }
    /// Whether a request turns reasoning on and off (the generation prompt opens the think block).
    pub fn reasoning_switch(&self) -> bool {
        self.think_start_id.is_none()
    }
    /// Whether a call's opening also ends the reasoning where a call may follow.
    pub fn calls_end_reasoning(&self) -> bool {
        self.think_start_id.is_none()
    }
}

pub const QWEN: ChatFormat = ChatFormat {
    vocabulary: 248320,
    stop_tokens: &[("<|endoftext|>", 248044), ("<|im_end|>", 248046)],
    think_end_id: 248069,
    think_start_id: None,
    call_open_id: 248058,
    call_syntax: QWEN_XML,
    message_boundary: "<|im_end|>",
};

pub const FLINT: ChatFormat = ChatFormat {
    vocabulary: 150144,
    stop_tokens: &[
        ("<|end_of_text|>", 150001),
        ("<|user|>", 150003),
        ("<|observation|>", 150005),
        ("<|developer|>", 150018),
    ],
    think_end_id: 150007,
    think_start_id: Some(150006),
    call_open_id: 150010,
    call_syntax: GLM_ARGS,
    message_boundary: "<|assistant|>",
};

/// A model family the engine serves (install/families.py).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    /// As the engine names it (runtime/model/*.hpp).
    pub name: &'static str,
    /// The DFlash2 draft released for the family.
    pub draft_repo: Option<&'static str>,
    pub chat_format: ChatFormat,
    pub vision: bool,
}

pub const FAMILIES: &[Family] = &[
    Family { name: "Qwen3.8-27B", draft_repo: Some("incoai/Qwen3.8-27B-DFlash2"), chat_format: QWEN, vision: true },
    Family { name: "Qwen3.6-35B-A3B", draft_repo: Some("incoai/Qwen3.6-35B-A3B-DFlash2"), chat_format: QWEN, vision: true },
    Family { name: "Qwen3.5-122B-A10B", draft_repo: Some("incoai/Ornith-1.5-122B-DFlash2"), chat_format: QWEN, vision: true },
    Family { name: "Qwen3.8-Flash-Next", draft_repo: Some("incoai/Qwen3.8-Flash-Next-DFlash2"), chat_format: QWEN, vision: true },
    Family { name: "Flint", draft_repo: Some("incoai/Flint-DFlash2"), chat_format: FLINT, vision: false },
];

pub fn family(name: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|f| f.name == name)
}
