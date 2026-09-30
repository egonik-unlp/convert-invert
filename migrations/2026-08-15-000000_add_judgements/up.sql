-- One row per (candidate, judge) verdict.
--
-- judge_submissions.score records *what* a judge decided; this table records who
-- decided it, under which model and prompt, how long it took, and — for the LLM
-- judge — the label that explains the number. Kept separate so several judges can
-- rule on the same candidate without widening judge_submissions once per judge.
CREATE TABLE IF NOT EXISTS judgements (
    id SERIAL PRIMARY KEY,
    submission INTEGER NOT NULL REFERENCES judge_submissions (id) ON DELETE CASCADE,
    judge VARCHAR NOT NULL,
    model VARCHAR,
    label VARCHAR,
    score FLOAT4 NOT NULL,
    accepted BOOLEAN NOT NULL DEFAULT FALSE,
    latency_ms INTEGER NOT NULL DEFAULT 0,
    prompt_version VARCHAR,
    raw TEXT,
    error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A candidate is re-judged on every run that reaches it; the latest verdict per
-- judge replaces the previous one rather than piling up duplicates.
CREATE UNIQUE INDEX IF NOT EXISTS judgements_submission_judge_key
    ON judgements (submission, judge);

-- The judgements feed reads newest-first, optionally narrowed to one label.
CREATE INDEX IF NOT EXISTS judgements_created_at_idx ON judgements (created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS judgements_judge_label_idx ON judgements (judge, label);
