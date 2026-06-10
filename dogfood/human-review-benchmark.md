# Human Review Benchmark

Lane: `bench-human-review-c`
Focus: conflict readability during `lane review`

## What Worked

The review output grouped competing edits by path and exposed the lane names, op ids, byte ranges, and next-step commands needed to inspect or resolve each conflict.

## Review Friction

Conflict groups do not include inline base or inserted text. A reviewer has to run `show-op` before they can compare the actual edit content.

## Follow-Up

Consider showing a short UTF-8 preview for each conflicted op directly in the review output, with `show-op` remaining the full-detail command.
