variable "region" {
  description = "AWS region for the artifact bucket"
  type        = string
  default     = "us-east-1"
}

variable "bucket_prefix" {
  description = "Prefix for the S3 bucket name (suffix includes account id + random hex for global uniqueness)"
  type        = string
  default     = "arcane-benchmark-artifacts"
}
