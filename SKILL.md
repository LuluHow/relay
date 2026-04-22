---
name: relay-orchestrate
description: "Helps decompose complex coding tasks into ordered sub-tasks and generates relay orchestration plan TOML files. Use this skill when the user wants to: break a big task into sequential Claude sessions, create a relay plan, orchestrate multiple agents on a project, or mentions 'relay orchestrate', 'plan.toml', 'multi-session', 'task decomposition', or 'orchestration plan'. Also trigger when the user describes a large refactor, migration, or multi-component feature that would benefit from being split into discrete steps executed by separate Claude sessions."
---

# relay-orchestrate

You help users decompose complex coding tasks into ordered sub-tasks and generate valid `plan.toml` files for `relay orchestrate`.

## What relay orchestrate does

`relay orchestrate plan.toml` reads a plan file, creates a single git branch and worktree, then runs `claude -p` sessions one at a time in that worktree. Each task sees the changes made by previous tasks, building up the work incrementally on one branch. A TUI monitors progress, and on completion the tool can merge the branch, create a PR, or leave it for manual merge.

The key idea: tasks run sequentially in a shared worktree. Task B sees everything task A committed. This means you can chain tasks that build on each other — no merge conflicts, no coordination headaches.

## Your workflow

### Step 1: Understand the task

Ask the user what they want to accomplish. If they've already described it, summarize your understanding back to them. You need to know:

- **The goal**: What's the end state? (e.g., "migrate from REST to GraphQL", "add i18n to the whole frontend")
- **The codebase**: What language, framework, rough structure? Read key files if needed.
- **Constraints**: Are there ordering dependencies? Does some work need to land before other work can begin?

### Step 2: Explore the codebase

Before decomposing, understand the project structure. Read the main entry points, directory layout, and key config files. You're looking for **natural stages** where work can be split into sequential steps:

- **By layer**: types/interfaces first, then implementation, then integration
- **By dependency order**: shared utilities before consumers, schema before resolvers
- **By feature area**: auth, billing, notifications — each as its own task
- **By phase**: prepare, transform, verify

### Step 3: Design the task sequence

Decompose into tasks following these principles:

**Order tasks by dependency.** Since each task runs after the previous one finishes and sees all prior changes, put foundational work (types, schemas, shared utilities) first and integration/verification tasks last. Use `depends_on` to express ordering constraints — a task won't start until its dependencies are done.

**Each task prompt must be self-contained.** Every Claude session starts with zero context. The prompt is all it gets. Include enough detail about the project structure, conventions, and what specifically to do. Reference file paths explicitly. Don't assume knowledge of other tasks.

**Right-size the tasks.** Too granular (20 tasks for 20 files) wastes overhead. Too coarse (2 tasks for everything) misses the benefit of discrete, focused sessions. Aim for 3-7 tasks for most projects. Each task should represent a coherent unit of work.

**Tell each task what already exists.** Since tasks build on each other, later prompts should reference what previous tasks created. For example: "The GraphQL schema has already been created in src/graphql/schema.graphql. Read it, then implement the resolvers."

**Common patterns:**
- Foundation task (schema/types/interfaces) -> implementation tasks -> integration task
- Independent feature slices in sequence
- Backend API then Frontend UI then integration
- Migration: prepare -> transform -> verify

### Step 4: Write the prompts

Each task prompt should include:

1. **Context**: Brief description of the project and what the user is trying to achieve overall
2. **Prior work**: What previous tasks have already done (files created, changes made)
3. **This task's scope**: Exactly what files/modules to modify and what changes to make
4. **Conventions**: Coding style, patterns to follow, frameworks in use
5. **Verification**: How to check the work (run tests, type check, etc.)

Good prompts are 100-300 words. Shorter prompts lead to guesswork; longer prompts lead to confusion.

### Step 5: Generate the TOML

Write the plan file and explain each task to the user. Validate:
- Task names are alphanumeric with hyphens/underscores only
- Dependencies reference existing tasks
- No circular dependencies
- `on_complete` is one of: `manual`, `merge`, `pr`
- `branch` is alphanumeric with hyphens/underscores

## Plan TOML schema

```toml
[plan]
name = "descriptive-plan-name"       # required, alphanumeric/-/_
model = "sonnet"                      # optional, default model for all tasks
on_complete = "manual"                # "manual" | "merge" | "pr"
branch = "my-feature-branch"          # the branch all tasks run on
skip_permissions = false              # allow --dangerously-skip-permissions

[[tasks]]
name = "task-name"                    # required, alphanumeric/-/_
prompt = """                          # required, the full prompt for claude -p
Your detailed task prompt here.
Can be multi-line.
"""
model = "opus"                        # optional, override plan-level model
depends_on = ["other-task"]           # optional, list of task names this waits for
allowed_tools = "Bash Edit Read"      # optional, restrict available tools
```

## Key decisions to help the user make

- **`on_complete`**: Use `manual` for first runs (inspect before merging). Use `merge` when confident. Use `pr` for team workflows with code review.
- **`skip_permissions`**: Only set to `true` if the prompts are trusted and you want fully autonomous execution. Default `false` means Claude will prompt for permission on file writes and shell commands — which blocks non-interactive `claude -p`. For most orchestration use cases, `skip_permissions = true` is necessary since there's no human to approve, but the user should understand the tradeoff.
- **`model`**: Use `sonnet` for straightforward tasks (tests, docs, simple refactors). Use `opus` for complex reasoning tasks (architecture changes, tricky migrations). Mix per-task to optimize cost.
- **`branch`**: Pick a descriptive branch name for the work. All tasks run here sequentially.

## Example: decomposing a REST-to-GraphQL migration

```toml
[plan]
name = "rest-to-graphql"
model = "sonnet"
on_complete = "manual"
branch = "feat-graphql"
skip_permissions = true

[[tasks]]
name = "schema-types"
prompt = """
Project: Node.js Express API in /src with TypeScript.
Goal: We're migrating from REST to GraphQL.

Your task: Create the GraphQL schema and TypeScript types.
- Read the existing REST routes in src/routes/ to understand the data model
- Create src/graphql/schema.graphql with types for User, Post, Comment
- Create src/graphql/types.ts with matching TypeScript interfaces
- Create src/graphql/resolvers/ directory structure (empty resolver files)

Do NOT modify any existing REST code — later tasks handle that.
Run `npx tsc --noEmit` to verify types compile.
"""

[[tasks]]
name = "user-resolvers"
depends_on = ["schema-types"]
prompt = """
Project: Node.js Express API in /src with TypeScript.
Goal: We're migrating from REST to GraphQL. The schema and types
have already been created in src/graphql/ by a previous task.

Your task: Implement the User resolvers.
- Read src/graphql/schema.graphql for the User type definition
- Implement resolvers in src/graphql/resolvers/user.ts
- Port logic from src/routes/users.ts (read it for reference, don't modify it)
- Use the existing database helpers in src/db/
- Add tests in src/graphql/__tests__/user.test.ts

Run `npm test -- --grep User` to verify.
"""

[[tasks]]
name = "post-resolvers"
depends_on = ["schema-types"]
prompt = """
Project: Node.js Express API in /src with TypeScript.
Goal: We're migrating from REST to GraphQL. The schema and types
have already been created in src/graphql/ by a previous task.

Your task: Implement the Post and Comment resolvers.
- Read src/graphql/schema.graphql for the Post and Comment types
- Implement resolvers in src/graphql/resolvers/post.ts
- Port logic from src/routes/posts.ts (read it, don't modify it)
- Handle nested Comment resolution
- Add tests in src/graphql/__tests__/post.test.ts

Run `npm test -- --grep Post` to verify.
"""

[[tasks]]
name = "server-integration"
depends_on = ["user-resolvers", "post-resolvers"]
prompt = """
Project: Node.js Express API in /src with TypeScript.
Goal: Integrate the new GraphQL endpoint into the Express server.
Previous tasks have created the schema, types, and all resolvers
in src/graphql/.

Your task: Wire up GraphQL as a new endpoint alongside existing REST.
- Install and configure apollo-server-express in src/server.ts
- Mount GraphQL at /graphql, keeping all existing REST routes at /api/*
- Import and combine all resolvers from src/graphql/resolvers/
- Add a health check that verifies the GraphQL schema loads
- Update package.json scripts if needed

Run `npm test` (full suite) to verify nothing broke.
"""
```

Notice: `user-resolvers` and `post-resolvers` both depend on `schema-types`, so they'll run after it completes (in TOML order). Then `server-integration` waits for both before running last. All four tasks execute in the same branch and worktree — each one picks up right where the last left off.
