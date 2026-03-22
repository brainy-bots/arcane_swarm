<#
.SYNOPSIS
  One-command AWS execution for Benchmark V2.

.DESCRIPTION
  Provisions a temporary EC2 instance, runs scripts/benchmark/Run-Benchmark-V2.ps1 remotely,
  uploads run artifacts to S3, downloads them locally, and destroys cloud resources.

  This script assumes:
  - AWS CLI is installed and configured locally.
  - You have permissions for EC2, IAM, SSM, and S3.
  - The benchmark repo is **public** (or otherwise cloneable without credentials).
  - Published Arcane infra + swarm images are **pullable without login** (public registry), passed via
    `-ArcaneInfraImage` / `-ArcaneSwarmImage` or `ARCANE_INFRA_IMAGE` / `ARCANE_SWARM_IMAGE`.

  Reproducible path: **public** benchmark repo + public images → no GitHub token.
  If the benchmark repo is still **private**, set `GITHUB_TOKEN` or `-GitHubToken` so the instance can `git clone` only
  (no submodules; images stay the published binaries).
#>
param(
  [Parameter(Mandatory=$true)]
  [string]$ArtifactBucket,

  [string]$Region = 'us-east-1',
  [string]$InstanceType = 'm6i.2xlarge',
  [string]$RepoUrl = 'https://github.com/martinjms/arcane-scaling-benchmarks.git',
  [string]$RepoRef = 'master',
  [string]$ArtifactPrefix = 'arcane-benchmarks/v2',
  [string]$LocalOutDir = '',

  [int]$StartPlayers = 250,
  [int]$StepPlayers = 250,
  [int]$MaxPlayers = 6000,
  [int]$DurationSeconds = 30,
  [int[]]$ArcaneClusterCounts = @(1,2,3,4,5,10),

  # Published images (public GHCR). Env: ARCANE_INFRA_IMAGE / ARCANE_SWARM_IMAGE. Defaults: martinjms …:v1.0.0.
  [string]$ArcaneInfraImage = '',
  [string]$ArcaneSwarmImage = '',

  # Only if RepoUrl is a private GitHub repo (clone). Use read-only PAT or `gh auth token` in GITHUB_TOKEN.
  [string]$GitHubToken = '',

  [switch]$KeepInstance,
  [switch]$KeepIamResources
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ArcaneInfraImage)) { $ArcaneInfraImage = $env:ARCANE_INFRA_IMAGE }
if ([string]::IsNullOrWhiteSpace($ArcaneSwarmImage)) { $ArcaneSwarmImage = $env:ARCANE_SWARM_IMAGE }
# Default published images (override with params or env for forks / new tags).
if ([string]::IsNullOrWhiteSpace($ArcaneInfraImage)) {
  $ArcaneInfraImage = 'ghcr.io/martinjms/arcane-benchmark-infra:v1.0.0'
}
if ([string]::IsNullOrWhiteSpace($ArcaneSwarmImage)) {
  $ArcaneSwarmImage = 'ghcr.io/martinjms/arcane-benchmark-swarm:v1.0.0'
}
if ($ArcaneInfraImage.Contains("'") -or $ArcaneSwarmImage.Contains("'")) {
  throw 'Image references must not contain single quotes (SSM shell escaping). Use a tag without apostrophes.'
}

$ghTokForClone = $GitHubToken
if ([string]::IsNullOrWhiteSpace($ghTokForClone)) { $ghTokForClone = $env:GITHUB_TOKEN }
$preCloneAuth = @()
if (-not [string]::IsNullOrWhiteSpace($ghTokForClone)) {
  if ($ghTokForClone -match "['`r`n]") { throw 'GitHubToken / GITHUB_TOKEN must not contain single quotes or newlines.' }
  $preCloneAuth += "export GITHUB_TOKEN='$ghTokForClone'"
  $preCloneAuth += 'git config --global url."https://x-access-token:${GITHUB_TOKEN}@github.com/".insteadOf "https://github.com/"'
}

function Invoke-AwsJson([string]$AwsArgs) {
  $raw = cmd /c "aws $AwsArgs --region $Region --output json"
  if ($LASTEXITCODE -ne 0) { throw "aws command failed: aws $AwsArgs" }
  if ([string]::IsNullOrWhiteSpace($raw)) { return $null }
  return ($raw | ConvertFrom-Json)
}

function Invoke-AwsText([string]$AwsArgs) {
  $raw = cmd /c "aws $AwsArgs --region $Region --output text"
  if ($LASTEXITCODE -ne 0) { throw "aws command failed: aws $AwsArgs" }
  return $raw.Trim()
}

function Wait-ForSsmOnline([string]$InstanceId) {
  for($i=0; $i -lt 90; $i++) {
    $ping = cmd /c "aws ssm describe-instance-information --region $Region --filters Key=InstanceIds,Values=$InstanceId --query `"InstanceInformationList[0].PingStatus`" --output text"
    if ($LASTEXITCODE -eq 0 -and $ping.Trim() -eq 'Online') { return }
    Start-Sleep -Seconds 5
  }
  throw "Instance $InstanceId did not become SSM Online in time."
}

function Wait-ForSsmCommand([string]$CommandId, [string]$InstanceId) {
  while ($true) {
    $status = cmd /c "aws ssm get-command-invocation --region $Region --command-id $CommandId --instance-id $InstanceId --query `"Status`" --output text"
    if ($LASTEXITCODE -ne 0) {
      Start-Sleep -Seconds 5
      continue
    }
    if ($status -in @('Success','Cancelled','TimedOut','Failed','Cancelling')) { return $status.Trim() }
    Start-Sleep -Seconds 10
  }
}

function Get-AwsFileUri([string]$Path) {
  $full = (Resolve-Path -LiteralPath $Path).Path -replace '\\','/'
  return "file://$full"
}

if ([string]::IsNullOrWhiteSpace($LocalOutDir)) {
  $LocalOutDir = Join-Path $PSScriptRoot ("aws_runs_" + (Get-Date -Format 'yyyyMMdd_HHmmss'))
}
$null = New-Item -ItemType Directory -Path $LocalOutDir -Force

Write-Host "Checking AWS CLI..." -ForegroundColor Cyan
cmd /c "aws sts get-caller-identity --region $Region --output text" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "AWS CLI not configured or credentials invalid." }

$runId = "arcane-v2-" + ([Guid]::NewGuid().ToString('N').Substring(0, 8))
$roleName = "$runId-role"
$profileName = "$runId-profile"
$sgName = "$runId-sg"

$instanceId = $null
$commandId = $null
$securityGroupId = $null

try {
  Write-Host "Resolving default VPC/subnet..." -ForegroundColor Cyan
  $vpcId = Invoke-AwsText "ec2 describe-vpcs --filters Name=isDefault,Values=true --query `"Vpcs[0].VpcId`""
  if ([string]::IsNullOrWhiteSpace($vpcId) -or $vpcId -eq 'None') { throw "No default VPC found in region $Region." }
  $subnetId = Invoke-AwsText "ec2 describe-subnets --filters Name=vpc-id,Values=$vpcId Name=default-for-az,Values=true --query `"Subnets[0].SubnetId`""
  if ([string]::IsNullOrWhiteSpace($subnetId) -or $subnetId -eq 'None') {
    $subnetId = Invoke-AwsText "ec2 describe-subnets --filters Name=vpc-id,Values=$vpcId --query `"Subnets[0].SubnetId`""
  }

  Write-Host "Creating security group..." -ForegroundColor Cyan
  $securityGroupId = Invoke-AwsText "ec2 create-security-group --group-name $sgName --description `"Arcane benchmark v2 ephemeral SG`" --vpc-id $vpcId --query `"GroupId`""

  Write-Host "Creating IAM role/profile..." -ForegroundColor Cyan
  $trustJson = '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}'
  $trustPath = Join-Path $env:TEMP "$runId-trust.json"
  $utf8NoBom = New-Object System.Text.UTF8Encoding $false
  [System.IO.File]::WriteAllText($trustPath, $trustJson, $utf8NoBom)
  $trustUri = Get-AwsFileUri -Path $trustPath
  & aws iam create-role --role-name $roleName --assume-role-policy-document $trustUri --output text | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Failed to create IAM role $roleName." }
  & aws iam attach-role-policy --role-name $roleName --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore --output text | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Failed to attach SSM policy to $roleName." }

  $s3Policy = @{
    Version = "2012-10-17"
    Statement = @(
      @{
        Effect   = "Allow"
        Action   = @("s3:PutObject","s3:AbortMultipartUpload","s3:ListBucket","s3:GetObject")
        Resource = @("arn:aws:s3:::$ArtifactBucket","arn:aws:s3:::$ArtifactBucket/*")
      }
    )
  } | ConvertTo-Json -Compress -Depth 5
  $s3PolicyPath = Join-Path $env:TEMP "$runId-s3-inline.json"
  [System.IO.File]::WriteAllText($s3PolicyPath, $s3Policy, $utf8NoBom)
  $s3PolicyUri = Get-AwsFileUri -Path $s3PolicyPath
  & aws iam put-role-policy --role-name $roleName --policy-name "$runId-s3" --policy-document $s3PolicyUri --output text | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Failed to attach inline S3 policy to $roleName." }

  cmd /c "aws iam create-instance-profile --instance-profile-name $profileName --output text" | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Failed to create instance profile $profileName." }
  cmd /c "aws iam add-role-to-instance-profile --instance-profile-name $profileName --role-name $roleName --output text" | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "Failed to add role $roleName to instance profile $profileName." }
  Start-Sleep -Seconds 10

  Write-Host "Resolving Ubuntu 22.04 AMI (Canonical)..." -ForegroundColor Cyan
  $amiId = Invoke-AwsText "ec2 describe-images --owners 099720109477 --filters Name=name,Values=ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-* Name=state,Values=available --query `"sort_by(Images, &CreationDate)[-1].ImageId`""
  if ([string]::IsNullOrWhiteSpace($amiId) -or $amiId -eq 'None') { throw "Failed to resolve Ubuntu 22.04 AMI via ec2 describe-images." }

  Write-Host "Launching EC2 instance ($InstanceType)..." -ForegroundColor Cyan
  $run = Invoke-AwsJson "ec2 run-instances --image-id $amiId --instance-type $InstanceType --iam-instance-profile Name=$profileName --subnet-id $subnetId --security-group-ids $securityGroupId --tag-specifications `"ResourceType=instance,Tags=[{Key=Name,Value=$runId}]`" --block-device-mappings `"DeviceName=/dev/sda1,Ebs={VolumeSize=100,VolumeType=gp3,DeleteOnTermination=true}`" --count 1"
  $instanceId = $run.Instances[0].InstanceId
  if ([string]::IsNullOrWhiteSpace($instanceId)) { throw "Failed to launch EC2 instance." }

  Write-Host "Waiting for instance running..." -ForegroundColor Cyan
  cmd /c "aws ec2 wait instance-running --region $Region --instance-ids $instanceId"
  if ($LASTEXITCODE -ne 0) { throw "Instance $instanceId failed to reach running state." }

  Write-Host "Waiting for SSM agent online..." -ForegroundColor Cyan
  Wait-ForSsmOnline -InstanceId $instanceId

  $remotePrefix = "$ArtifactPrefix/$runId"
  $clusterCsv = ($ArcaneClusterCounts -join ',')

  $remoteCommands = @(
    # SSM AWS-RunShellScript uses /bin/sh (dash on Ubuntu), not bash — no pipefail.
    "set -eu",
    "export HOME=/root",
    "export DEBIAN_FRONTEND=noninteractive",
    "echo debconf debconf/frontend select Noninteractive | sudo debconf-set-selections",
    "sudo apt-get update",
    "sudo apt-get install -y ca-certificates curl git jq unzip software-properties-common",
    "sudo apt-get install -y binaryen || true",
    "curl -fsSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o /tmp/awscliv2.zip",
    "unzip -q -o /tmp/awscliv2.zip -d /tmp",
    "sudo /tmp/aws/install --update",
    "rm -rf /tmp/aws /tmp/awscliv2.zip",
    "curl -fsSL https://download.docker.com/linux/ubuntu/gpg | sudo gpg --dearmor -o /usr/share/keyrings/docker.gpg",
    "echo `"deb [arch=`$(dpkg --print-architecture) signed-by=/usr/share/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu `$(. /etc/os-release && echo `$VERSION_CODENAME) stable`" | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null",
    "sudo apt-get update",
    "sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin",
    # Host `spacetime build` (Run-Benchmark-V2.ps1) needs Rust + wasm32 target on the instance.
    "sudo apt-get install -y build-essential pkg-config libssl-dev",
    "curl -sSf https://install.spacetimedb.com | sh -s -- -y",
    "curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable",
    # Do not source ~/.bashrc under SSM: document runs as /bin/sh (dash); bashrc uses bash-only builtins (shopt).
    'export PATH="/root/.cargo/bin:/root/.local/bin:/root/.spacetime/bin:$PATH"',
    "rustup target add wasm32-unknown-unknown",
    "curl -L -o /tmp/powershell.deb https://github.com/PowerShell/PowerShell/releases/download/v7.4.6/powershell_7.4.6-1.deb_amd64.deb",
    "sudo dpkg -i /tmp/powershell.deb || sudo apt-get -f install -y",
    "rm -f /tmp/powershell.deb"
  ) + $preCloneAuth + @(
    "mkdir -p /opt/bench && cd /opt/bench",
    "git clone --depth 1 --branch $RepoRef $RepoUrl repo",
    "cd repo",
    "cd scripts/benchmark",
    "export AWS_DEFAULT_REGION=$Region",
    "export ARCANE_INFRA_IMAGE='$ArcaneInfraImage'",
    "export ARCANE_SWARM_IMAGE='$ArcaneSwarmImage'",
    'export PATH="/root/.cargo/bin:/root/.local/bin:/root/.spacetime/bin:$PATH"',
    # Note: remote runner is /bin/sh — do not use PowerShell @( ) here; pass comma-separated ints to pwsh.
    "pwsh -NoLogo -NoProfile -File ./Run-Benchmark-V2.ps1 -UsePublishedImages -StartPlayers $StartPlayers -StepPlayers $StepPlayers -MaxPlayers $MaxPlayers -DurationSeconds $DurationSeconds -ArcaneClusterCounts $clusterCsv",
    'LATEST_DIR=$(ls -dt v2_runs_* | head -n 1)',
    'if [ -z "$LATEST_DIR" ]; then echo ''No v2 run output found'' >&2; exit 1; fi',
    "aws s3 cp `"./`$LATEST_DIR`" `"s3://$ArtifactBucket/$remotePrefix/`$LATEST_DIR`" --recursive --region $Region",
    "echo `"s3://$ArtifactBucket/$remotePrefix/`$LATEST_DIR`" | tee /tmp/arcane_v2_s3_path.txt",
    "aws s3 cp /tmp/arcane_v2_s3_path.txt `"s3://$ArtifactBucket/$remotePrefix/s3_path.txt`" --region $Region"
  )

  Write-Host "Starting remote benchmark command via SSM..." -ForegroundColor Cyan
  $ssmParamsPath = Join-Path $env:TEMP "$runId-ssm-parameters.json"
  $utf8NoBom2 = New-Object System.Text.UTF8Encoding $false
  $ssmParamsOnly = @{ commands = $remoteCommands }
  [System.IO.File]::WriteAllText($ssmParamsPath, ($ssmParamsOnly | ConvertTo-Json -Depth 12), $utf8NoBom2)
  $ssmParamsUri = Get-AwsFileUri -Path $ssmParamsPath
  $commandId = (& aws ssm send-command --region $Region --instance-ids $instanceId --document-name AWS-RunShellScript --comment 'Arcane benchmark v2 cloud run' --parameters $ssmParamsUri --output json --query 'Command.CommandId' --output text).Trim()
  if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($commandId) -or $commandId -eq 'None') { throw "Failed to start SSM send-command (parameters file)." }

  Write-Host "Running benchmark remotely (this can take a while)..." -ForegroundColor Yellow
  $status = Wait-ForSsmCommand -CommandId $commandId -InstanceId $instanceId
  if ($status -ne 'Success') {
    $stderr = cmd /c "aws ssm get-command-invocation --region $Region --command-id $commandId --instance-id $instanceId --query `"StandardErrorContent`" --output text"
    throw "Remote benchmark command status: $status`n$stderr"
  }

  Write-Host "Fetching artifact location..." -ForegroundColor Cyan
  $s3Path = Invoke-AwsText "s3 cp s3://$ArtifactBucket/$remotePrefix/s3_path.txt -"
  if ([string]::IsNullOrWhiteSpace($s3Path)) { throw "Could not fetch artifact S3 path marker." }
  $s3Path = $s3Path.Trim()

  Write-Host "Downloading artifacts to $LocalOutDir..." -ForegroundColor Cyan
  cmd /c "aws s3 cp $s3Path `"$LocalOutDir`" --recursive --region $Region"
  if ($LASTEXITCODE -ne 0) { throw "Failed to download artifacts from $s3Path." }

  Write-Host "Cloud benchmark completed successfully." -ForegroundColor Green
  Write-Host "Artifacts: $LocalOutDir" -ForegroundColor Green
  Write-Host "Remote S3:  $s3Path" -ForegroundColor Green
}
finally {
  if ($instanceId -and -not $KeepInstance) {
    Write-Host "Terminating EC2 instance $instanceId ..." -ForegroundColor DarkGray
    try { cmd /c "aws ec2 terminate-instances --region $Region --instance-ids $instanceId --output text" | Out-Null } catch {}
    try { cmd /c "aws ec2 wait instance-terminated --region $Region --instance-ids $instanceId" | Out-Null } catch {}
  }

  if ($securityGroupId) {
    Write-Host "Deleting security group $securityGroupId ..." -ForegroundColor DarkGray
    try { cmd /c "aws ec2 delete-security-group --region $Region --group-id $securityGroupId --output text" | Out-Null } catch {}
  }

  if (-not $KeepIamResources) {
    Write-Host "Cleaning up IAM role/profile..." -ForegroundColor DarkGray
    try { cmd /c "aws iam remove-role-from-instance-profile --instance-profile-name $profileName --role-name $roleName --output text" | Out-Null } catch {}
    try { cmd /c "aws iam delete-instance-profile --instance-profile-name $profileName --output text" | Out-Null } catch {}
    try { cmd /c "aws iam delete-role-policy --role-name $roleName --policy-name $($runId)-s3 --output text" | Out-Null } catch {}
    try { cmd /c "aws iam detach-role-policy --role-name $roleName --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore --output text" | Out-Null } catch {}
    try { cmd /c "aws iam delete-role --role-name $roleName --output text" | Out-Null } catch {}
  }
}
