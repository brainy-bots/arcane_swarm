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

## Important parameters

- `-ArtifactBucket` (required): S3 bucket for run outputs.
- `-Region` (default `us-east-1`)
- `-InstanceType` (default `m6i.2xlarge`)
- `-RepoUrl` / `-RepoRef` (public benchmark repo; shallow clone, **no submodules**)
- `-ArcaneInfraImage` / `-ArcaneSwarmImage` (or `ARCANE_INFRA_IMAGE` / `ARCANE_SWARM_IMAGE`)
- `-StartPlayers`, `-StepPlayers`, `-MaxPlayers`, `-DurationSeconds`, `-ArcaneClusterCounts`

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
