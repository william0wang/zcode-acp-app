// @vitest-environment node
// The session panel's current-model fallback: a value that missed the option
// list must still read as a model NAME, not the raw `provider\model` or
// `custom:…` spelling it arrived as. Same logic as the config screens' labels
// (bare id unless qualified), expressed against the two wire spellings.
import { describe, expect, it } from "vitest";
import {
  bareModelIdFromConfigValue,
  configValueMatchKey,
  currentChoice,
} from "../src/lib/modelValue";

describe("bareModelIdFromConfigValue", () => {
  it("strips the provider from the bridge's backslash spelling", () => {
    expect(
      bareModelIdFromConfigValue("builtin:bigmodel-coding-plan\\GLM-5.3"),
    ).toBe("GLM-5.3");
    expect(bareModelIdFromConfigValue("uuid-1\\deepseek-v4")).toBe(
      "deepseek-v4",
    );
  });

  it("decodes the agent-definition custom: spelling", () => {
    expect(
      bareModelIdFromConfigValue(
        "custom:account%3Abigmodel-individual-coding-plan:GLM-5.3",
      ),
    ).toBe("GLM-5.3");
    // A malformed escape degrades to the raw segment, not a throw.
    expect(bareModelIdFromConfigValue("custom:prov:bad%")).toBe("bad%");
  });

  it("passes a bare value through unchanged", () => {
    // Mode and thought options use bare words; they must not be mangled.
    expect(bareModelIdFromConfigValue("max")).toBe("max");
    expect(bareModelIdFromConfigValue("build")).toBe("build");
  });
});

describe("configValueMatchKey / currentChoice", () => {
  const options = [
    { value: "builtin:bigmodel-coding-plan\\GLM-5.3", name: "GLM 5.3" },
    { value: "uuid-1\\deepseek-v4", name: "DeepSeek V4" },
  ];

  it("folds the account: coding-plan dialect onto builtin:", () => {
    // The runtime's mid-turn push vs the bridge's option list — same model,
    // two provider spellings. The raw strings never match; the keys do.
    expect(configValueMatchKey("account:bigmodel-individual-coding-plan\\GLM-5.3")).toBe(
      configValueMatchKey("builtin:bigmodel-coding-plan\\GLM-5.3"),
    );
    // Non-coding-plan and bare spellings compare as-is.
    expect(configValueMatchKey("uuid-1\\deepseek-v4")).toBe("uuid-1\\deepseek-v4");
    expect(configValueMatchKey("max")).toBe("max");
  });

  it("currentChoice resolves the choice name across the dialect swap", () => {
    expect(
      currentChoice({ options, currentValue: "builtin:bigmodel-coding-plan\\GLM-5.3" })
        ?.name,
    ).toBe("GLM 5.3");
    expect(
      currentChoice({ options, currentValue: "account:bigmodel-team-coding-plan\\GLM-5.3" })
        ?.name,
    ).toBe("GLM 5.3");
    // A value outside the list resolves to nothing — the caller falls back.
    expect(currentChoice({ options, currentValue: "uuid-2\\glm-4.6" })).toBeUndefined();
    expect(currentChoice({ options, currentValue: null })).toBeUndefined();
  });
});
