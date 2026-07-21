CREATE TABLE "owners" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "projects" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "repositorys" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "worktrees" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "sessions" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "jobs" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    PRIMARY KEY ("id")
);
CREATE TABLE "rich_records" (
    "id" BLOB NOT NULL CHECK (length(CAST("id" AS BLOB)) = 16),
    "owner_id" BLOB NOT NULL CHECK (length(CAST("owner_id" AS BLOB)) = 16),
    "project_id" BLOB NOT NULL CHECK (length(CAST("project_id" AS BLOB)) = 16),
    "repository_id" BLOB NOT NULL CHECK (length(CAST("repository_id" AS BLOB)) = 16),
    "worktree_id" BLOB NOT NULL CHECK (length(CAST("worktree_id" AS BLOB)) = 16),
    "session_id" BLOB NOT NULL CHECK (length(CAST("session_id" AS BLOB)) = 16),
    "job_id" BLOB NOT NULL CHECK (length(CAST("job_id" AS BLOB)) = 16),
    "name" TEXT NOT NULL,
    "slug" TEXT NOT NULL,
    "kind" TEXT NOT NULL,
    "stage" TEXT NOT NULL,
    "outcome" TEXT NOT NULL,
    "status" TEXT NOT NULL DEFAULT 'queued',
    "source" TEXT NOT NULL,
    "target" TEXT NOT NULL,
    "label" TEXT,
    "detail" TEXT,
    "attempts" INTEGER NOT NULL DEFAULT 0,
    "priority" INTEGER NOT NULL,
    "created_at" INTEGER NOT NULL,
    "updated_at" INTEGER NOT NULL,
    "active" INTEGER NOT NULL DEFAULT 1,
    "archived" INTEGER NOT NULL,
    "payload" BLOB NOT NULL,
    "digest" BLOB NOT NULL CHECK (length(CAST("digest" AS BLOB)) = 32),
    "optional_digest" BLOB CHECK (length(CAST("optional_digest" AS BLOB)) = 32),
    "optional_payload" BLOB,
    "optional_count" INTEGER,
    PRIMARY KEY ("id"),
    UNIQUE ("name"),
    UNIQUE ("slug"),
    UNIQUE ("kind"),
    UNIQUE ("source"),
    UNIQUE ("target"),
    UNIQUE ("label"),
    UNIQUE ("detail"),
    UNIQUE ("digest"),
    UNIQUE ("stage", "outcome"),
    CHECK ((length("owner_id") = 16)),
    CHECK ((length("project_id") = 16)),
    CHECK ((length("repository_id") = 16)),
    CHECK ((length("worktree_id") = 16)),
    CHECK ((length("session_id") = 16)),
    CHECK ((length("job_id") = 16)),
    CHECK (("name" <> '')),
    CHECK (("slug" <> '')),
    CHECK (("kind" <> '')),
    CHECK (("stage" <> '')),
    CHECK (("outcome" <> '')),
    CHECK (("status" <> '')),
    CHECK (("source" <> '')),
    CHECK (("target" <> '')),
    CHECK ((("label" IS NULL) OR ("label" <> ''))),
    CHECK ((("detail" IS NULL) OR ("detail" <> ''))),
    CHECK (("attempts" >= 0)),
    CHECK (("priority" >= 0)),
    CHECK (("created_at" >= 0)),
    CHECK (("updated_at" >= 0)),
    CHECK (("active" IN (0, 1))),
    CHECK (("archived" IN (0, 1))),
    CHECK ((length("digest") = 32)),
    CHECK ((("optional_digest" IS NULL) OR (length("optional_digest") = 32))),
    CHECK ((("optional_payload" IS NULL) OR (length("optional_payload") > 0))),
    CHECK ((("optional_count" IS NULL) OR ("optional_count" >= 0))),
    FOREIGN KEY ("owner_id") REFERENCES "owners" ("id") ON DELETE CASCADE,
    FOREIGN KEY ("project_id") REFERENCES "projects" ("id") ON DELETE CASCADE,
    FOREIGN KEY ("repository_id") REFERENCES "repositorys" ("id") ON DELETE CASCADE,
    FOREIGN KEY ("worktree_id") REFERENCES "worktrees" ("id") ON DELETE CASCADE,
    FOREIGN KEY ("session_id") REFERENCES "sessions" ("id") ON DELETE CASCADE,
    FOREIGN KEY ("job_id") REFERENCES "jobs" ("id") ON DELETE CASCADE
);
CREATE INDEX "idx_records_id" ON "rich_records" ("id");
CREATE INDEX "idx_records_owner" ON "rich_records" ("owner_id");
CREATE INDEX "idx_records_project" ON "rich_records" ("project_id");
CREATE INDEX "idx_records_repository" ON "rich_records" ("repository_id");
CREATE INDEX "idx_records_worktree" ON "rich_records" ("worktree_id");
CREATE INDEX "idx_records_session" ON "rich_records" ("session_id");
CREATE INDEX "idx_records_job" ON "rich_records" ("job_id");
CREATE INDEX "idx_records_outcome_stage" ON "rich_records" ("outcome", "stage");
