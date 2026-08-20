# Shared compute

Run work on other machines' hardware. A prompt goes to whichever device has
the model and the GPU; a render goes to whichever one is idle. The network
behaves like one larger machine.

**Contents**

1. [What can be shared](#1-what-can-be-shared)
2. [Turning it on](#2-turning-it-on)
3. [Pairing devices](#3-pairing-devices)
4. [Running models](#4-running-models)
5. [Declaring tasks](#5-declaring-tasks)
6. [The orchestrator](#6-the-orchestrator)
7. [The security model](#7-the-security-model)
8. [The API](#8-the-api)
9. [When it does not work](#9-when-it-does-not-work)

---

## 1. What can be shared

Every node broadcasts what it has, so the whole network knows what is
available without asking:

| | |
|---|---|
| **GPUs** | Name, VRAM and vendor. Discrete cards are marked as accelerators; integrated graphics are detected but never advertised as one |
| **Models** | Whatever a local [Ollama](https://ollama.com) has pulled |
| **Tasks** | Named commands the machine's owner has declared — renders, simulations, transcodes, anything |
| **CPU and memory** | Threads and free RAM |
| **Capacity** | How many jobs are running, and how many more it will take |

On a laptop with an RTX 4060, another node sees:

```
peer: workshop-nuc  compute_enabled=True
  GPU:   NVIDIA GeForce RTX 4060 Laptop GPU  vram=8188 MB  vendor=nvidia
  GPU:   Intel(R) UHD Graphics               vram=2047 MB  vendor=intel
  model: llama3.2:3b
  task:  render params=['scene', 'output']
  threads=24 ram=18432MB slots=2 pairing=True
```

> VRAM comes from `nvidia-smi` where it exists. Windows' own `AdapterRAM` is a
> 32-bit field that silently reports an 8 GB card as 4 GB, so it is only used
> as a fallback.

---

## 2. Turning it on

**Compute is off by default.** Chat and files are open to the whole LAN
because they cost nothing but bytes; compute spends someone's electricity,
memory and time, so it is opened deliberately.

In `~/.bifrost/config.json`:

```json
{
  "compute_enabled": true,
  "compute_require_pairing": true,
  "compute_max_jobs": 2,
  "orchestrator_enabled": true
}
```

Or from the dashboard, under **Compute**. Both switches are live:

| Setting | Effect |
|---|---|
| `compute_enabled` | Whether this machine runs work **for others** |
| `orchestrator_enabled` | Whether this machine places work **onto others** |
| `compute_max_jobs` | How many jobs it will run at once |
| `compute_require_pairing` | Whether a peer must be approved first |

They are independent. A laptop can submit work to the network without
offering any of its own, and a headless server can offer everything without
ever submitting.

---

## 3. Pairing devices

Nothing runs on your hardware until you have explicitly approved the device
asking.

**On the machine that will do the work**, open its dashboard → Compute →
**Pair a device**. A six-digit code appears:

```
  913746        expires in 3 minutes
```

**On the machine that wants to use it**, enter that code. From then on it is
remembered — across restarts — and never asks again.

```sh
# or from the API
curl -X POST localhost:9000/api/compute/pair/begin        # on the sharer
curl -X POST localhost:9000/api/compute/pair/with \
     -H 'Content-Type: application/json' \
     -d '{"peer":"workshop-nuc","code":"913746"}'         # on the asker
```

The code is safe at six digits because of what surrounds it: it expires in
three minutes, a wrong guess burns an attempt, five failures cancel it
entirely, and it pairs exactly one device. That is a one-in-two-hundred-
thousand chance for an attacker who is **already on your network**.

To withdraw access later, Compute → the device → **Revoke**.

**Pairing is one-directional.** Reading a code off a workstation lets *you* use
*it*. It does not let that workstation use your laptop; for that, it pairs with
you in turn, off a code you show. The two halves are stored separately —
`compute_tokens` is what this machine presents to others, `compute_trusted` is
who may use this one — and only the second grants anything locally.

Setting `compute_require_pairing: false` opens this machine's declared tasks
to anyone on the network. Reasonable on a lab network you fully control;
never on Wi-Fi you share.

---

## 4. Running models

Install [Ollama](https://ollama.com) on whichever machines have the GPUs and
pull whatever you want:

```sh
ollama pull llama3.2:3b
ollama pull qwen2.5-coder:7b
```

Bifrost finds it automatically — no configuration — and advertises every model
to the network. Any device can then use them, **including a phone**:

```sh
curl -X POST localhost:9000/api/compute/run \
     -H 'Content-Type: application/json' \
     -d '{"model":"llama3.2:3b","prompt":"explain TCP backpressure","priority":"interactive"}'
```

```json
{
  "job_id": "…",
  "planned_for": "workshop-nuc",
  "reason": "2 slot(s) free · NVIDIA GeForce RTX 4060 Laptop GPU · 8 GB",
  "text": "Queued for workshop-nuc — 2 slot(s) free · NVIDIA GeForce RTX 4060 Laptop GPU · 8 GB"
}
```

The answer streams back token by token; poll
`/api/compute/job/<job_id>` or watch it appear in the dashboard.

Bifrost never loads a model itself. Ollama already solves the hard parts —
weights, GPU offload, quantisation, memory — and exposes them over HTTP;
Bifrost finds it, lists what it has, and relays prompts to it.

---

## 5. Declaring tasks

A task is a command **template** written into the config of the machine that
will run it. This is how anything other than a model gets shared.

```json
{
  "compute_enabled": true,
  "tasks": {
    "render": {
      "command": "blender",
      "args": ["-b", "{scene}", "-o", "{output}", "-f", "{frame}"],
      "description": "Render one Blender frame",
      "needs_gpu": true,
      "timeout_secs": 7200
    },
    "simulate": {
      "command": "python",
      "args": ["/opt/sims/run.py", "--config", "{config}", "--steps", "{steps}"],
      "description": "Run a simulation",
      "working_dir": "/opt/sims"
    },
    "transcode": {
      "command": "ffmpeg",
      "args": ["-i", "{input}", "-c:v", "libx265", "{output}"],
      "description": "Transcode to H.265"
    }
  }
}
```

Placeholders are discovered from the template automatically and advertised, so
other machines know what to supply. Running one:

```sh
curl -X POST localhost:9000/api/compute/run \
     -H 'Content-Type: application/json' \
     -d '{"task":"render","parameters":{"scene":"/shared/a.blend","output":"/out/","frame":"120"},"needs_gpu":true}'
```

Output streams back line by line as it is produced, so a long job reports
progress rather than going silent for an hour.

### What a task may and may not do

The remote node supplies **values**, never the command. It cannot choose the
program, add arguments, or reach a shell — see [§7](#7-the-security-model).

Each task also runs with a cleared environment except `PATH`, because a task
running on behalf of a peer has no business seeing that machine's secrets.

---

## 6. The orchestrator

Every node runs one. There is deliberately **no elected leader and no central
bus process**: a single coordinator would be a single point of failure on a
network whose whole premise is that machines come and go. Close the laptop
that happens to be coordinating and the cluster would stop.

Instead each node places the work *it* submits, using the capacity everyone
already broadcasts. Placement is therefore local, instant, and survives any
other machine disappearing.

### How a machine is chosen

Candidates are scored, not filtered:

| Factor | Weight | Why |
|---|---|---|
| Free job slots | **highest** | A fast machine at its limit finishes later than a slower idle one |
| A discrete GPU | high | Decides whether the work is viable at all |
| VRAM | medium | Decides which models fit |
| CPU threads, RAM | low | Tie-breakers |
| Being this machine | slight penalty | Offloading is the point, and it keeps the machine you are sitting at responsive |

The reason is reported back, so the dashboard can explain the choice rather
than the work vanishing into the network.

### Priorities

Work is queued, not fired blindly:

| Priority | For |
|---|---|
| `interactive` | Someone is waiting — a prompt typed into a dashboard |
| `normal` | The default |
| `background` | Overnight work; yields to everything else |

An interactive prompt overtakes a queued render, which is the difference
between a cluster that feels responsive and one that merely has capacity.

### When two nodes pick the same worker

They can, and that is fine. The worker enforces its own concurrency limit and
refuses the surplus; the submitting node falls through to its next choice.
Preventing it with distributed locking would cost far more than the occasional
retry. After four refusals a job gives up with a clear error rather than
looping forever.

### Toggling it

```sh
curl -X POST localhost:9000/api/compute/orchestrator \
     -H 'Content-Type: application/json' -d '{"enabled":false}'
```

Switching it off stops new placement but lets running work finish — killing
someone's half-completed render because a toggle moved would be worse.

---

## 7. The security model

Two rules, and everything else follows from them.

### Nothing runs that the owner did not declare

A peer names a model or a task and fills in placeholders. It cannot supply a
command line.

Every substituted value becomes **exactly one `argv` entry**, and there is no
shell anywhere in the execution path. A parameter containing
`; rm -rf / #` is passed to the program as that literal string:

```
args = ["-b", "; rm -rf / #", "-o", "out/"]     # four arguments, as declared
```

Values are also rejected if they contain a NUL byte (which truncates what the
operating system actually receives) or a line break (which corrupts any
line-oriented protocol the task itself speaks). Shell metacharacters are
*allowed*, because they are harmless without a shell and a filename may
legitimately contain a semicolon.

The alternative design — accepting command lines from the network — is a
remote code execution service on your LAN. Any device that reached the port,
or any device later compromised, would own every machine that trusted it.

### Nothing runs for a peer the owner did not approve

Pairing, as above. Unlike chat and files, compute is closed by default.

### What this does not protect against

Be clear-eyed:

- **A declared task is as dangerous as the command in it.** Declaring
  `{"command": "bash", "args": ["-c", "{script}"]}` hands away the machine.
  Do not do that.
- **There is no encryption.** Prompts and job output cross the LAN in plain
  text, like everything else in Bifrost.
- **A paired device is trusted until revoked.** If it is stolen or
  compromised, revoke it.
- **Resource limits are per-job-count, not per-CPU.** A task can still use the
  whole machine while it runs; `compute_max_jobs` bounds how many at once, not
  how hard each pushes.

---

## 7a. From a terminal

Everything the dashboard does, the same code path drives from a command line —
which is what a script, a cron job or an ssh session has.

```sh
bifrost compute                 # what the network can run, and where
bifrost compute on              # share this machine's hardware
bifrost pair                    # show a code for another device to type
bifrost pair workshop-nuc 913746
bifrost submit llama3.2:3b "summarise this in one line"
bifrost submit render scene=/data/city.blend output=/tmp/frame
bifrost jobs                    # queued, running, finished, and why one failed
```

```
$ bifrost compute
1 device(s) sharing · 1 accelerator(s) · 10235 MB VRAM · 24 threads · 2 slot(s) free
  workshop-nuc — 0/2 busy · NVIDIA GeForce RTX 4060 Laptop GPU · 8 GB
      models: llama3.2:3b, qwen2.5-coder:7b
      tasks:  render, wordcount
```

Inside either dashboard the same things are `/compute`, `/pair`, `/run` and
`/jobs`. It is one implementation: the command a phone taps, a terminal types
and the REST API posts all reach the same `Node::execute`.

`run` already means "start a node here", so the command-line verb for work
that goes out to the network is `submit`.

**A machine never pairs with itself.** With one computer, a GPU and a model,
every job is placed locally — pairing applies to *other* devices.

---

## 8. The API

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/compute` | GET | Everything: capacity, jobs, settings, paired devices |
| `/api/compute/toggle` | POST | `{"enabled":true}` — share this machine |
| `/api/compute/orchestrator` | POST | `{"enabled":true}` — place work on others |
| `/api/compute/pair/begin` | POST | Show a pairing code |
| `/api/compute/pair/with` | POST | `{"peer":"…","code":"913746"}` |
| `/api/compute/revoke` | POST | `{"peer":"…"}` |
| `/api/compute/run` | POST | Submit work |
| `/api/compute/job/<id>` | GET | A job and its output so far |
| `/api/compute/job/<id>` | DELETE | Cancel |

`/api/compute` returns the pooled view:

```json
{
  "enabled": true,
  "orchestrator_enabled": true,
  "capacity": {
    "nodes": 3, "accelerators": 2, "total_vram_mb": 20480,
    "cpu_threads": 48, "free_slots": 5,
    "models": ["llama3.2:3b", "qwen2.5-coder:7b"],
    "tasks": ["render", "transcode"]
  },
  "trusted": [{"node_id": "…", "display_name": "workshop-nuc", "jobs_run": 12}],
  "jobs": [ … ]
}
```

---

## 9. When it does not work

### "No device on this network can run that"

The orchestrator found nothing capable. Check, in order:

1. The device offering it has `compute_enabled: true`.
2. The model or task name matches exactly — `GET /api/compute` lists what the
   network actually has.
3. If the task has `needs_gpu`, some device has a **discrete** GPU. Integrated
   graphics deliberately do not count.
4. The devices can see each other at all: `bifrost peers`.

### "This device has not been paired with that one"

Pair them, from [§3](#3-pairing-devices). This is the expected error when a
device tries to use resources it has not been approved for.

### "This device is already at its job limit"

Raise `compute_max_jobs`, or wait. The submitting node retries elsewhere
automatically.

### Models are not showing up

Bifrost looks for Ollama on `127.0.0.1:11434`:

```sh
curl localhost:11434/api/tags      # should list your models
ollama serve                       # if it is not running
```

`OLLAMA_HOST` is honoured if you run it on a different port.

Note that a model is advertised by the machine Ollama is *on*. Installing
Ollama on the machine doing the asking does nothing for the network.

### A task fails immediately

Its output is returned verbatim — read it. The usual causes are the program
not being on `PATH` (the environment is cleared except `PATH`, so a tool
installed in a shell profile will not be found), a missing parameter, or a
working directory that does not exist.

### Nothing at all is happening

The compute port is **9006**, and the installers opened it. If you added
firewall rules by hand before this feature existed, re-run `bifrost doctor` —
it now checks 9006 too.
