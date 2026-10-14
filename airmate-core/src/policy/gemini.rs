//! The Gemini `generateContent` inbound field policy, as of 2026-10-14.
//!
//! `airmate fields --inbound gemini` prints this table. The Gemini REST API accepts each field in
//! camelCase or snake_case; names here are camelCase and the parser normalises snake_case to match.
//! Adding a translation is a row here plus its handling in the parser, in the same change.

use super::{
    entry,
    Class::{Ignored, Rejected, Translated},
    Entry,
    Group::{
        BlockField, ContentBlock, MessageKey, Request, StopReason, ThinkingField, ToolChoiceField,
        ToolField,
    },
};

const NO_EQUIVALENT: &str = "no upstream equivalent";
const SERVER_SIDE: &str = "server-side tools are not executed by airmate";

/// The Gemini inbound policy table.
pub const TABLE: &[Entry] = &[
    // Request fields
    entry(Request, "contents", Translated, "the conversation"),
    entry(
        Request,
        "systemInstruction",
        Translated,
        "the system prompt",
    ),
    entry(Request, "tools", Translated, "function declarations"),
    entry(
        Request,
        "toolConfig",
        Translated,
        "`functionCallingConfig` selects the tool-choice mode",
    ),
    entry(
        Request,
        "generationConfig",
        Translated,
        "sampling, limits, structured output and thinking (see the `generationConfig.` rows)",
    ),
    entry(Request, "safetySettings", Ignored, NO_EQUIVALENT),
    entry(Request, "labels", Ignored, NO_EQUIVALENT),
    entry(Request, "serviceTier", Ignored, NO_EQUIVALENT),
    entry(
        Request,
        "cachedContent",
        Rejected,
        "cached content lives in Google's service and cannot be read by another upstream",
    ),
    entry(
        Request,
        "generationConfig.temperature",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "generationConfig.topP",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "generationConfig.topK",
        Translated,
        "sent to Gemini, Fireworks and generic upstreams",
    ),
    entry(
        Request,
        "generationConfig.candidateCount",
        Translated,
        "only `1`; more than one candidate is rejected",
    ),
    entry(
        Request,
        "generationConfig.maxOutputTokens",
        Translated,
        "the output token limit",
    ),
    entry(
        Request,
        "generationConfig.stopSequences",
        Translated,
        "stop sequences",
    ),
    entry(
        Request,
        "generationConfig.seed",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "generationConfig.presencePenalty",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "generationConfig.frequencyPenalty",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "generationConfig.responseMimeType",
        Translated,
        "`application/json` asks for JSON; `text/plain` is the default",
    ),
    entry(
        Request,
        "generationConfig.responseSchema",
        Translated,
        "an OpenAPI-subset schema for structured output",
    ),
    entry(
        Request,
        "generationConfig.responseJsonSchema",
        Translated,
        "a JSON Schema for structured output",
    ),
    entry(
        Request,
        "generationConfig.thinkingConfig",
        Translated,
        "reasoning on, off or with a budget (see the thinking rows)",
    ),
    entry(
        Request,
        "generationConfig.responseModalities",
        Translated,
        "only `TEXT`; image or audio output is rejected",
    ),
    entry(
        Request,
        "generationConfig.responseLogprobs",
        Rejected,
        "log probabilities cannot be produced",
    ),
    entry(
        Request,
        "generationConfig.logprobs",
        Rejected,
        "log probabilities cannot be produced",
    ),
    entry(
        Request,
        "generationConfig.speechConfig",
        Rejected,
        "audio output is not supported",
    ),
    entry(
        Request,
        "generationConfig.mediaResolution",
        Ignored,
        NO_EQUIVALENT,
    ),
    entry(
        Request,
        "generationConfig.audioTimestamp",
        Ignored,
        NO_EQUIVALENT,
    ),
    entry(
        Request,
        "generationConfig.enableEnhancedCivicAnswers",
        Ignored,
        NO_EQUIVALENT,
    ),
    // Message keys
    entry(
        MessageKey,
        "role",
        Translated,
        "`user` or `model` (a missing role is `user`)",
    ),
    entry(MessageKey, "parts", Translated, "the content parts"),
    // Content parts
    entry(ContentBlock, "text", Translated, "text"),
    entry(
        ContentBlock,
        "inlineData",
        Translated,
        "an image, a PDF or text, as base64",
    ),
    entry(
        ContentBlock,
        "fileData",
        Rejected,
        "file URIs point at Google's service; send the bytes inline",
    ),
    entry(
        ContentBlock,
        "functionCall",
        Translated,
        "a tool call made by the model",
    ),
    entry(
        ContentBlock,
        "functionResponse",
        Translated,
        "a tool result",
    ),
    entry(
        ContentBlock,
        "executableCode",
        Rejected,
        "code execution is a server-side tool",
    ),
    entry(
        ContentBlock,
        "codeExecutionResult",
        Rejected,
        "code execution is a server-side tool",
    ),
    entry(
        BlockField,
        "text.thought",
        Ignored,
        "reasoning is not replayed on Gemini requests",
    ),
    entry(
        BlockField,
        "thoughtSignature",
        Ignored,
        "reasoning is not replayed on Gemini requests",
    ),
    entry(BlockField, "videoMetadata", Ignored, NO_EQUIVALENT),
    // Tool fields
    entry(
        ToolField,
        "functionDeclarations",
        Translated,
        "function tools",
    ),
    entry(ToolField, "googleSearch", Rejected, SERVER_SIDE),
    entry(ToolField, "googleSearchRetrieval", Rejected, SERVER_SIDE),
    entry(ToolField, "codeExecution", Rejected, SERVER_SIDE),
    entry(ToolField, "urlContext", Rejected, SERVER_SIDE),
    entry(ToolField, "retrieval", Rejected, SERVER_SIDE),
    entry(ToolField, "computerUse", Rejected, SERVER_SIDE),
    entry(ToolField, "fileSearch", Rejected, SERVER_SIDE),
    entry(ToolField, "googleMaps", Rejected, SERVER_SIDE),
    entry(ToolField, "name", Translated, "the function name"),
    entry(
        ToolField,
        "description",
        Translated,
        "the function description",
    ),
    entry(
        ToolField,
        "parameters",
        Translated,
        "an OpenAPI-subset schema, converted to JSON Schema",
    ),
    entry(
        ToolField,
        "parametersJsonSchema",
        Translated,
        "a JSON Schema for the arguments",
    ),
    entry(ToolField, "response", Ignored, NO_EQUIVALENT),
    entry(ToolField, "responseJsonSchema", Ignored, NO_EQUIVALENT),
    entry(
        ToolField,
        "behavior",
        Ignored,
        "asynchronous function calls are not supported",
    ),
    // Function calling config
    entry(
        ToolChoiceField,
        "mode",
        Translated,
        "`AUTO`, `ANY`, `NONE` or `VALIDATED`",
    ),
    entry(
        ToolChoiceField,
        "allowedFunctionNames",
        Translated,
        "one name forces that function; several are treated as `ANY` and reported",
    ),
    // Thinking config
    entry(
        ThinkingField,
        "includeThoughts",
        Translated,
        "thought summaries are returned only when set",
    ),
    entry(
        ThinkingField,
        "thinkingBudget",
        Translated,
        "`-1` lets the model decide, `0` turns thinking off, otherwise a token budget",
    ),
    entry(
        ThinkingField,
        "thinkingLevel",
        Translated,
        "an effort level (`minimal`, `low`, `medium`, `high`)",
    ),
    // Finish reasons
    entry(
        StopReason,
        "STOP",
        Translated,
        "from `end_turn` or `tool_use`",
    ),
    entry(StopReason, "MAX_TOKENS", Translated, "from `max_tokens`"),
    entry(
        StopReason,
        "SAFETY",
        Translated,
        "from a refusal or content filter",
    ),
];
