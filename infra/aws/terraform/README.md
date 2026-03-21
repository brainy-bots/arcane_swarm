# Terraform: benchmark artifact bucket (AWS)

Creates a **private, versioned, SSE-S3–encrypted** S3 bucket for `Run-Benchmark-V2-Aws.ps1` outputs.

This is the **durable** part of the cloud setup. The PowerShell runner still provisions **ephemeral** EC2 + IAM for each run (until we split that into Terraform/ECS later).

## Prerequisites

- [Terraform](https://www.terraform.io/downloads) ≥ 1.3
- AWS credentials with permission to create S3 buckets

## Usage

```bash
cd infra/aws/terraform
terraform init
terraform plan -var="region=us-east-1"
terraform apply -var="region=us-east-1"
```

Copy `artifact_bucket_name` from outputs and pass it to:

```powershell
cd ../../../scripts/cloud
.\Run-Benchmark-V2-Aws.ps1 -ArtifactBucket <bucket-from-terraform-output> -Region us-east-1
```

## Destroy

```bash
terraform destroy -var="region=us-east-1"
```

**Warning:** `destroy` deletes the bucket (and objects if empty or after force). Export anything you need from S3 first.
