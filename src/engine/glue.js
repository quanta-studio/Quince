// Runs inside the embedded JavaScript engine, ahead of any spec. This file is the whole world a
// spec's functions can reach: there is no file system, network or timer here, only what the
// Rust side passes in. It mirrors how Fig itself runs generators.
//
// The Rust side provides two functions:
//   __spawn(run, input)  starts a command for a run and returns the command's number
//   __finish(run, items) reports a run's suggestions as JSON text, or null if it failed
"use strict";
(() => {
  // Generators stay here, since their functions cannot cross into Rust. The Rust side refers
  // to them by position.
  const generators = [null];
  const numbers = new Map();
  // Commands that are running, and how to hand their output back to whoever awaits them.
  const waiting = new Map();

  const silent = () => {};
  globalThis.console = { log: silent, info: silent, warn: silent, error: silent, debug: silent, trace: silent };

  const keep = (generator) => {
    if (!numbers.has(generator)) {
      numbers.set(generator, generators.push(generator) - 1);
    }
    return numbers.get(generator);
  };

  const staticPart = (value) => (typeof value === "function" ? { fn: true } : value);

  const describeGenerator = (generator) => ({
    __gen: keep(generator),
    runnable: typeof generator.custom === "function" || Boolean(generator.script),
    template: generator.template,
    filterTemplate: typeof generator.filterTemplateSuggestions === "function",
    trigger: staticPart(generator.trigger),
    getQueryTerm: staticPart(generator.getQueryTerm),
  });

  // Turns a spec into the JSON text the Rust side reads. Functions are left out, and each
  // generator is replaced by a description of the parts that are not functions.
  globalThis.__describe = (spec) => {
    if (spec === null || typeof spec !== "object") {
      return null;
    }
    return JSON.stringify(spec, (key, value) => {
      if (key === "generators" && value !== null && typeof value === "object") {
        const list = Array.isArray(value) ? value : [value];
        return list.filter((item) => item !== null && typeof item === "object").map(describeGenerator);
      }
      return typeof value === "function" ? undefined : value;
    });
  };

  // Takes in a generator written as plain JSON, which can have a script but no functions.
  globalThis.__register = (text) => keep(JSON.parse(text));

  // Fig hands command output to specs tidied like this, and specs are written to count on it:
  // many split on newlines and would trip over a trailing one.
  const tidy = (text) =>
    String(text ?? "")
      .replaceAll("\r\n", "\n")
      .replace(/\x1b\[\?25h/g, "")
      .replace(/^\n+/, "")
      .replace(/\n+$/, "");

  const normalise = (items) => {
    if (!Array.isArray(items)) {
      return [];
    }
    return items
      .filter((item) => item !== null && item !== undefined && item !== "")
      .map((item) => (typeof item === "string" ? { name: item } : item));
  };

  const run = async (generator, tokens, context, execute) => {
    if (typeof generator.custom === "function") {
      return generator.custom(tokens, execute, context);
    }
    const script = typeof generator.script === "function" ? generator.script(tokens) : generator.script;
    if (!script) {
      return [];
    }
    const input = Array.isArray(script) ? { command: script[0], args: script.slice(1) } : { ...script };
    if (input.timeout === undefined && typeof generator.scriptTimeout === "number") {
      input.timeout = generator.scriptTimeout;
    }
    const { stdout } = await execute(input);
    if (generator.splitOn !== undefined) {
      return stdout.trim() === "" ? [] : stdout.trim().split(generator.splitOn);
    }
    if (typeof generator.postProcess === "function") {
      return generator.postProcess(stdout, tokens);
    }
    return [];
  };

  globalThis.__start = (runNumber, generatorNumber, tokensText, contextText) => {
    const execute = (input) =>
      new Promise((resolve) => {
        waiting.set(__spawn(runNumber, JSON.stringify(input)), resolve);
      });
    const report = (items) => {
      let text = null;
      try {
        text = JSON.stringify(normalise(items));
      } catch (_) {}
      __finish(runNumber, text);
    };
    run(generators[generatorNumber], JSON.parse(tokensText), JSON.parse(contextText), execute).then(report, () =>
      __finish(runNumber, null),
    );
  };

  globalThis.__resolve = (commandNumber, outputText) => {
    const resolve = waiting.get(commandNumber);
    if (resolve) {
      waiting.delete(commandNumber);
      const output = JSON.parse(outputText);
      resolve({ ...output, stdout: tidy(output.stdout), stderr: tidy(output.stderr) });
    }
  };

  globalThis.__trigger = (generatorNumber, token, previous) => {
    const trigger = generators[generatorNumber].trigger;
    return typeof trigger === "function" ? Boolean(trigger(token, previous)) : false;
  };

  globalThis.__queryTerm = (generatorNumber, token) => {
    const getQueryTerm = generators[generatorNumber].getQueryTerm;
    const term = typeof getQueryTerm === "function" ? getQueryTerm(token) : token;
    return typeof term === "string" ? term : token;
  };

  globalThis.__filterTemplate = (generatorNumber, itemsText) => {
    const filter = generators[generatorNumber].filterTemplateSuggestions;
    const items = JSON.parse(itemsText);
    return JSON.stringify(normalise(typeof filter === "function" ? filter(items) : items));
  };
})();
