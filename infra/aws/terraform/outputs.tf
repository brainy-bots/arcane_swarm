output "artifact_bucket_name" {
  description = "Pass this to Run-Benchmark-V2-Aws.ps1 -ArtifactBucket"
  value       = aws_s3_bucket.benchmark_artifacts.id
}

output "artifact_bucket_arn" {
  value = aws_s3_bucket.benchmark_artifacts.arn
}
