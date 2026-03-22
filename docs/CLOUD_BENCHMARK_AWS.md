# Cloud Benchmark (AWS, One Command)

This guide runs Benchmark v2 on a temporary AWS EC2 host and pulls results back locally.

## Infrastructure layout (script vs Terraform)

| Piece | Tool | Notes |
|--------|------|--------|
| **S3 artifact bucket** | **Terraform** (recommended) | `infra/aws/terraform/` — private, versioned, encrypted bucket; reviewable `plan`/`apply`. |
| **Ephemeral EC2 + IAM + SSM run** | **PowerShell + AWS CLI** | `scripts/cloud/Run-Benchmark-V2-Aws.ps1` — creates a throwaway instance profile and security group each run. |

Next evolution: move the EC2/SSM pieces into Terraform or ECS so the whole path is declarative; the bucket module is step one.

## What this does

`scripts/cloud/Run-Benchmark-V2-Aws.ps1` will:

1. Create ephemeral AWS resources (security group, IAM role/profile, EC2 instance).
2. Install Docker, PowerShell, and SpacetimeDB CLI on the EC2 instance.
3. Clone this benchmark repo (**shallow**, no submodules — public repo only).
4. Run `scripts/benchmark/Run-Benchmark-V2.ps1 -UsePublishedImages` with published Arcane infra + swarm images.
5. Upload benchmark outputs to S3.
6. Download outputs to your local machine.
7. Destroy AWS resources (unless you opt out).

## Prerequisites

- AWS CLI installed and configured locally (`aws sts get-caller-identity` works).
- Permissions for: EC2, SSM, IAM, S3.
- An S3 bucket for artifacts — **create with Terraform** (see `infra/aws/terraform/README.md`) or use an existing bucket name.
- **Published container images** for Benchmark v2 (no private GitHub access on the instance):
  - **`ARCANE_INFRA_IMAGE`** — image containing `arcane-manager` and `arcane-cluster` (same as `docker/Dockerfile.arcane-infra`).
  - **`ARCANE_SWARM_IMAGE`** — image containing `arcane-swarm` (same as `docker/Dockerfile.swarm`).

  Pass them as parameters or environment variables before running the cloud script:

  ```powershell
  $env:ARCANE_INFRA_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-infra:v1.0.0'
  $env:ARCANE_SWARM_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-swarm:v1.0.0'
  ```

  Images must be **public** (or pullable without logging in on the EC2 host). See **Publishing images (maintainers)** below.

### Optional: create the bucket with Terraform

```bash
cd infra/aws/terraform
terraform init
terraform apply -var="region=us-east-1"
```

Use the `artifact_bucket_name` output as `-ArtifactBucket`.

## One-command run

From repo root:

```powershell
cd scripts\cloud
$env:ARCANE_INFRA_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-infra:v1.0.0'
$env:ARCANE_SWARM_IMAGE = 'ghcr.io/martinjms/arcane-benchmark-swarm:v1.0.0'
.\Run-Benchmark-V2-Aws.ps1 -ArtifactBucket <your-s3-bucket> -Region us-east-1
```

Or pass `-ArcaneInfraImage` / `-ArcaneSwarmImage` instead of env vars.

Outputs are downloaded under:

- `scripts/cloud/aws_runs_<timestamp>/...`

## How you get results (by design: wait, then see everything)

The cloud script is **synchronous**: it blocks until the remote benchmark finishes (often **30+ minutes** for a full sweep), then:

1. **Exit code** — `0` means the remote run and S3 upload/download succeeded; non-zero means inspect the error text.
2. **Terminal** — After download, it prints **`benchmark_v2_results.csv` as a table** so you see ceilings without opening a file first.
3. **Local folder** — Full run tree: CSV, `logs/`, `metrics/`, `.env.v2` under `aws_runs_<timestamp>/`.
4. **S3** — The same files stay in your bucket under `arcane-benchmarks/v2/...` so you can share a prefix with collaborators or re-download with `aws s3 cp`.

We intentionally **do not** fire-and-forget: reproducibility runs need a clear pass/fail and artifacts on disk. If you need **async** runs (e.g. start from CI and pick results up later), use the same S3 prefix pattern or add your own workflow that only polls S3.

**Local** `Run-Benchmark-V2.ps1` already waits and prints a table at the end; results are under `scripts/benchmark/v2_runs_<timestamp>/`.

### Progress and container resources (while the run is in flight)

- The orchestrator **polls SSM** every `-SsmPollSeconds` (default 12) and prints **any new** `StandardOutputContent` / `StandardErrorContent` from the instance. You also get a **heartbeat** line with SSM status and elapsed time.
- **Caveat:** AWS often **buffers** remote script output until a subprocess exits or a line-buffer boundary, so you may see **bursts** rather than smooth streaming. For guaranteed live tailing you’d enable **CloudWatch Logs** on `send-command` and `aws logs tail` in another terminal (not wired in this script yet).
- On the instance, `Run-Benchmark-V2.ps1` is called with **`-DockerStatsLogIntervalSec`** (default **90** on the cloud script). That prints periodic **`docker stats`** tables (CPU, memory, net I/O per container) into the same remote log, so they show up in your terminal when SSM flushes. Set **`-DockerStatsLogIntervalSec 0`** on `Run-Benchmark-V2-Aws.ps1` to turn that off.

## Important parameters

- `-ArtifactBucket` (required): S3 bucket for run outputs.
- `-Region` (default `us-east-1`)
- `-InstanceType` (default `m6i.2xlarge`)
- `-RepoUrl` / `-RepoRef` (public benchmark repo; shallow clone, **no submodules**)
- `-ArcaneInfraImage` / `-ArcaneSwarmImage` (or `ARCANE_INFRA_IMAGE` / `ARCANE_SWARM_IMAGE`)
- `-StartPlayers`, `-StepPlayers`, `-MaxPlayers`, `-DurationSeconds`, `-ArcaneClusterCounts`
- `-SsmPollSeconds` (how often to pull remote logs / heartbeat)
- `-DockerStatsLogIntervalSec` (remote `docker stats` snapshot period; `0` disables)

## Why the first run feels slow

Each invocation launches a **fresh EC2** and runs a **single long SSM script**. Before the benchmark loop you pay for:

1. **Package installs** — apt, Docker, AWS CLI v2 zip, PowerShell `.deb`, **Rust + `wasm32-unknown-unknown`**, Spacetime CLI.
2. **Large `docker pull`s** — especially `clockworklabs/spacetime:latest` and Redis (hundreds of MB over the network).
3. **`spacetime build`** on the instance — compiles your module to WASM (CPU-bound).

**“Minimum” benchmark flags** only shorten the **last** phase; they do **not** skip bootstrap. Expect **roughly 15–35+ minutes** even for a small sweep, depending on region/network and cache. **SSM stdout/stderr often stay empty for long stretches** (buffering); heartbeat lines from the orchestrator mean the remote command is still **InProgress**, not stuck.

To make repeat runs fast, use a **custom AMI** or **pre-baked image** with Docker, Rust, Spacetime, and PowerShell already installed (not automated in this repo yet).

## Cost and cleanup notes

- The script terminates the EC2 instance and removes temporary IAM/SG resources by default.
- To debug a failed run:
  - add `-KeepInstance` to keep EC2 alive,
  - add `-KeepIamResources` to keep IAM role/profile alive.
- S3 artifacts remain in your bucket unless deleted manually.

## Publishing images (maintainers)

Third parties do **not** need access to private `arcane` source. Publish two images (version tags that match your paper / release notes), for example to **GHCR** (`ghcr.io`).

### Option A — GitHub Actions (recommended)

Workflow: [`.github/workflows/publish-benchmark-images.yml`](../.github/workflows/publish-benchmark-images.yml).

- **Actions → Publish benchmark images → Run workflow** — set the image tag (e.g. `v1.0.0`).
- Or push a git tag: `benchmark-images/v1.0.0` (creates images tagged `v1.0.0`).

If the **`arcane` submodule** is a **private** repo, add a repository secret **`ARCANE_SUBMODULE_PAT`** (read-only access to that repo). If `arcane` is public, no extra secret is needed.

After the first push, open **GitHub → your profile/org → Packages**, select each package (`arcane-benchmark-infra`, `arcane-benchmark-swarm`), **Package settings → Change visibility → Public** so others can `docker pull` without logging in.

### Option B — Local script (after `docker login ghcr.io`)

From repo root, with `arcane/` initialized:

```powershell
.\scripts\docker\Publish-Images.ps1 -Owner <your-github-username-or-org> -Tag v1.0.0
```

Use a GitHub PAT with `write:packages` when `docker login ghcr.io` prompts for a password.

### Option C — Manual `docker build` / `push`

1. **Infra** (needs `arcane/` submodule):

   ```bash
   docker build -f docker/Dockerfile.arcane-infra -t ghcr.io/<owner>/arcane-benchmark-infra:<tag> .
   docker push ghcr.io/<owner>/arcane-benchmark-infra:<tag>
   ```

2. **Swarm** (benchmark repo only):

   ```bash
   docker build -f docker/Dockerfile.swarm -t ghcr.io/<owner>/arcane-benchmark-swarm:<tag> .
   docker push ghcr.io/<owner>/arcane-benchmark-swarm:<tag>
   ```

`<owner>` must be **lowercase** for GHCR.

Document the exact `<tag>` (or digest) next to published results so others can pin the same bits.

Local runs without submodules: `Run-Benchmark-V2.ps1 -UsePublishedImages` with the same env vars — see [REPRODUCIBILITY.md](../REPRODUCIBILITY.md).

## Known limits of this first cloud runner

- Uses one EC2 host for the full benchmark run (not yet one component per machine).
- Uses your selected instance size to improve reproducibility compared to a busy local workstation.
- Next step (recommended): split services into separate AWS runtime units (ECS/EC2 or EKS) while preserving the same benchmark harness.
