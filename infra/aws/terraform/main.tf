provider "aws" {
  region = var.region
}

data "aws_caller_identity" "current" {}

resource "random_id" "bucket_suffix" {
  byte_length = 2
}

resource "aws_s3_bucket" "benchmark_artifacts" {
  bucket = "${var.bucket_prefix}-${data.aws_caller_identity.current.account_id}-${random_id.bucket_suffix.hex}"

  tags = {
    Project = "arcane-scaling-benchmarks"
    Purpose = "v2-cloud-run-artifacts"
  }
}

resource "aws_s3_bucket_public_access_block" "benchmark_artifacts" {
  bucket = aws_s3_bucket.benchmark_artifacts.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_versioning" "benchmark_artifacts" {
  bucket = aws_s3_bucket.benchmark_artifacts.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "benchmark_artifacts" {
  bucket = aws_s3_bucket.benchmark_artifacts.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}
