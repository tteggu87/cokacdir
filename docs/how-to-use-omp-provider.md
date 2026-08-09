# How to Use the OMP Provider

The OMP provider runs the local Oh My Pi CLI agent (`omp`) from cokacdir.
It is available alongside the Claude, Codex, Agy, and OpenCode providers.
Select it with `/model omp:<model>` in the same way that you select a Codex
model with its provider prefix.

## Prerequisites

Install the Oh My Pi CLI and make sure the `omp` executable is available on
`PATH` for the account that runs cokacdir.

Confirm the installation before starting the bot:

```bash
omp --version
```

The startup provider report shows `omp ✓` when cokacdir can find the binary.

## Switching to OMP

Use `/model` with the `omp:` prefix followed by an OMP model ID:

```text
/model omp:anthropic/claude-sonnet-4-5
/model omp:openai/gpt-5.1-codex
/model omp:google/gemini-3-pro-preview
```

The available model IDs depend on the providers configured in your local OMP
installation. Use the IDs accepted by your installed `omp` version.

After switching, ordinary chat messages are sent to OMP until you select a
different model.

## Session Behavior

cokacdir keeps an OMP session for each workspace. After the first request,
later requests in that workspace resume the saved session automatically by
invoking OMP with `-r`.

Changing workspaces uses that workspace's own session rather than sharing the
conversation across projects.

## Using a Specific OMP Binary

Set `COKAC_OMP_PATH` when `omp` is not on `PATH`, or when cokacdir should use a
particular installation:

```bash
export COKAC_OMP_PATH="$HOME/bin/omp"
cokacdir --ccserver
```

The value must point to an executable OMP binary. Set the variable in the
environment of the service account when cokacdir runs under a supervisor.

## Security

The bot executes OMP with the same filesystem, command, and network permissions
as the local CLI process. OMP can therefore act with the permissions of the
account running cokacdir. Restrict the bot to your own chat and do not expose
it to untrusted users.

## Troubleshooting

### The startup report shows `omp ✗`

cokacdir could not find the OMP executable on `PATH`. Run `omp --version` in
the same environment as cokacdir, or set `COKAC_OMP_PATH` to the full binary
path.

### The first response is slow

The first response can take 5–10 seconds while the OMP process starts and
initializes its local session. Resumed requests normally avoid most of that
startup work.
