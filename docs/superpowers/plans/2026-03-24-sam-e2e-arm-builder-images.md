# SAM E2E ARM Builder Images Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the SAM E2E Dockerfiles use explicit ARM builder images so CI builds follow the same architecture end to end.

**Architecture:** Keep the current SAM image-based E2E flow, but align the Docker builder stage with the existing `arm64` template and runtime stage. Guard that contract with focused tests in the existing SAM E2E workflow assertions.

**Tech Stack:** SAM CLI, Docker, Amazon Linux 2023 Lambda images, Python `unittest`

---

## Chunk 1: Guard the ARM image contract

### Task 1: Add failing Dockerfile assertions

**Files:**
- Modify: `tests/test_sam_invoke_e2e.py`
- Test: `tests/test_sam_invoke_e2e.py`

- [ ] **Step 1: Write the failing test**

Add assertions that each SAM Dockerfile contains an explicit ARM builder base image and still uses the ARM Lambda runtime image.

- [ ] **Step 2: Run test to verify it fails**

Run: `python3 -B tests/test_sam_invoke_e2e.py`
Expected: FAIL because the current builder `FROM` lines still use the generic Amazon Linux base image.

## Chunk 2: Align builder images

### Task 2: Update SAM Dockerfiles to ARM-specific builder bases

**Files:**
- Modify: `sam/write_lambda.Dockerfile`
- Modify: `sam/index_builder_lambda.Dockerfile`
- Modify: `sam/query_lambda.Dockerfile`
- Test: `tests/test_sam_invoke_e2e.py`

- [ ] **Step 1: Write minimal implementation**

Change the builder `FROM` line in each Dockerfile to the ARM-specific Amazon Linux 2023 image while leaving the rest of the build flow unchanged.

- [ ] **Step 2: Run test to verify it passes**

Run: `python3 -B tests/test_sam_invoke_e2e.py`
Expected: PASS

## Chunk 3: Focused verification

### Task 3: Re-run workflow guard coverage

**Files:**
- Test: `tests/test_ci_workflow.py`
- Test: `tests/test_sam_invoke_e2e.py`

- [ ] **Step 1: Run focused verification**

Run: `python3 -B tests/test_ci_workflow.py && python3 -B tests/test_sam_invoke_e2e.py`
Expected: both commands pass.

- [ ] **Step 2: Commit**

```bash
git add docs/superpowers/plans/2026-03-24-sam-e2e-arm-builder-images.md tests/test_sam_invoke_e2e.py sam/write_lambda.Dockerfile sam/index_builder_lambda.Dockerfile sam/query_lambda.Dockerfile
git commit -m "fix: align SAM E2E images to arm64"
```
