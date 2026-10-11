// A spec in the form Fig publishes them, with generators of each kind, for the end-to-end tests.
const list = ["printf", "apple\nbanana\ncherry"];

export default {
  name: "fruit",
  description: "Things that grow on trees",
  subcommands: [
    {
      name: "pick",
      description: "Takes a script's output through postProcess",
      options: [{ name: "--all", description: "Every one of them" }],
      args: {
        name: "fruit",
        suggestions: [{ name: "kiwi", description: "Written in the spec" }],
        generators: {
          script: list,
          postProcess: (out, tokens) => out.split("\n").map((name) => ({ name, description: `From ${tokens[0]}` })),
        },
      },
    },
    {
      name: "late",
      description: "Suggestions that take a while",
      options: [{ name: "--all", description: "Every one of them" }],
      args: {
        suggestions: ["kiwi"],
        generators: { script: ["sh", "-c", "sleep 1.5; printf 'apple\nbanana'"], splitOn: "\n" },
      },
    },
    {
      name: "stuck",
      description: "Suggestions that never arrive in time",
      options: [{ name: ["-c", "--create"], description: "Make a new one", args: { name: "name" } }],
      args: { suggestions: ["main"], generators: { script: ["sleep", "4"], splitOn: "\n" } },
    },
    {
      name: "env",
      description: "Shows what environment a generator's command runs in",
      args: { generators: { script: ["sh", "-c", "printf '%s\\n' \"$FRUIT_KIND\" \"${PWD##*/}\""], splitOn: "\n" } },
    },
    {
      name: "tree",
      description: "A custom generator that completes one level of a path at a time",
      args: {
        generators: {
          custom: async (tokens, execute, context) => {
            const typed = tokens[tokens.length - 1];
            const dir = typed.slice(0, typed.lastIndexOf("/") + 1);
            const { stdout } = await execute({ command: "ls", args: ["-p", `${context.currentWorkingDirectory}/${dir}`] });
            return stdout.split("\n").filter(Boolean).map((name) => ({ name }));
          },
          trigger: (token, previous) => token.lastIndexOf("/") !== previous.lastIndexOf("/"),
          getQueryTerm: (token) => token.slice(token.lastIndexOf("/") + 1),
        },
      },
    },
  ],
};
