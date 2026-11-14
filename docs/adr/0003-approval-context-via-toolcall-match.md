# Approval card context comes from the tool_call, matched by toolCallId

`session/request_permission` params carry almost no context the user can act
on: options are bare labels and `toolCall.rawInput` is an opaque
tool-specific object. The plan text (ExitPlanMode) and the question text
(AskUserQuestion) are not in the params at all — the bridge ships them as the
content of a `tool_call` update sent immediately before the request. We
therefore resolve approval context by matching `params.toolCall.toolCallId`
against the message stream, instead of asking the server to fatten the
permission params. Cost: the card must degrade to plain buttons when the
matching tool_call is missing (e.g. it arrived before we attached).

2026-11 amendment: the degradation is no longer total for AskUserQuestion —
the bridge's request_permission fallback always carries the question on the
wire (`toolCall.title` = the prompt text, `rawInput.questions` = the question
set, the toolCallId's `_N` suffix = the asked question's index), and the card
rebuilds the question view from those when the part is missing.
