-- RustLLMroute schema, release 1. Portable between SQLite and PostgreSQL:
-- timestamps are BIGINT epoch milliseconds (UTC), days are TEXT 'YYYY-MM-DD',
-- JSON and lists are TEXT, every integer is BIGINT, no RETURNING, ids are generated client-side.

CREATE TABLE IF NOT EXISTS keys (
  token                 TEXT PRIMARY KEY,
  key_name              TEXT,
  key_alias             TEXT UNIQUE,
  user_id               TEXT,
  team_id               TEXT,
  spend                 DOUBLE PRECISION NOT NULL DEFAULT 0,
  max_budget            DOUBLE PRECISION,
  soft_budget           DOUBLE PRECISION,
  budget_duration       TEXT,
  budget_reset_at       BIGINT,
  expires_at            BIGINT,
  models                TEXT NOT NULL DEFAULT '[]',
  aliases               TEXT NOT NULL DEFAULT '{}',
  model_max_budget      TEXT NOT NULL DEFAULT '{}',
  allowed_routes        TEXT NOT NULL DEFAULT '[]',
  metadata              TEXT NOT NULL DEFAULT '{}',
  max_parallel_requests BIGINT,
  tpm_limit             BIGINT,
  rpm_limit             BIGINT,
  blocked               BOOLEAN NOT NULL DEFAULT FALSE,
  created_at            BIGINT NOT NULL,
  updated_at            BIGINT NOT NULL,
  last_active_at        BIGINT,
  created_by            TEXT
);
CREATE INDEX IF NOT EXISTS idx_keys_user_id ON keys(user_id);
CREATE INDEX IF NOT EXISTS idx_keys_team_id ON keys(team_id);
CREATE INDEX IF NOT EXISTS idx_keys_reset ON keys(budget_reset_at) WHERE budget_reset_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS teams (
  team_id               TEXT PRIMARY KEY,
  team_alias            TEXT UNIQUE,
  members               TEXT NOT NULL DEFAULT '[]',
  spend                 DOUBLE PRECISION NOT NULL DEFAULT 0,
  max_budget            DOUBLE PRECISION,
  budget_duration       TEXT,
  budget_reset_at       BIGINT,
  models                TEXT NOT NULL DEFAULT '[]',
  model_max_budget      TEXT NOT NULL DEFAULT '{}',
  metadata              TEXT NOT NULL DEFAULT '{}',
  max_parallel_requests BIGINT,
  tpm_limit             BIGINT,
  rpm_limit             BIGINT,
  blocked               BOOLEAN NOT NULL DEFAULT FALSE,
  created_at            BIGINT NOT NULL,
  updated_at            BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_teams_reset ON teams(budget_reset_at) WHERE budget_reset_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS users (
  user_id               TEXT PRIMARY KEY,
  user_email            TEXT UNIQUE,
  user_alias            TEXT,
  user_role             TEXT NOT NULL DEFAULT 'internal_user',
  spend                 DOUBLE PRECISION NOT NULL DEFAULT 0,
  max_budget            DOUBLE PRECISION,
  budget_duration       TEXT,
  budget_reset_at       BIGINT,
  models                TEXT NOT NULL DEFAULT '[]',
  metadata              TEXT NOT NULL DEFAULT '{}',
  max_parallel_requests BIGINT,
  tpm_limit             BIGINT,
  rpm_limit             BIGINT,
  created_at            BIGINT NOT NULL,
  updated_at            BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_users_reset ON users(budget_reset_at) WHERE budget_reset_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS spend_logs (
  request_id            TEXT PRIMARY KEY,
  call_type             TEXT NOT NULL,
  api_key               TEXT NOT NULL DEFAULT '',
  key_alias             TEXT,
  user_id               TEXT,
  team_id               TEXT,
  end_user              TEXT,
  spend                 DOUBLE PRECISION NOT NULL DEFAULT 0,
  total_tokens          BIGINT NOT NULL DEFAULT 0,
  prompt_tokens         BIGINT NOT NULL DEFAULT 0,
  completion_tokens     BIGINT NOT NULL DEFAULT 0,
  cache_read_input_tokens     BIGINT NOT NULL DEFAULT 0,
  cache_creation_input_tokens BIGINT NOT NULL DEFAULT 0,
  start_time            BIGINT NOT NULL,
  end_time              BIGINT NOT NULL,
  completion_start_time BIGINT,
  request_duration_ms   BIGINT,
  model                 TEXT NOT NULL DEFAULT '',
  model_id              TEXT,
  model_group           TEXT,
  custom_llm_provider   TEXT,
  api_base              TEXT,
  status                TEXT NOT NULL DEFAULT 'success',
  error                 TEXT,
  cache_hit             BOOLEAN NOT NULL DEFAULT FALSE,
  stream                BOOLEAN NOT NULL DEFAULT FALSE,
  partial               BOOLEAN NOT NULL DEFAULT FALSE,
  request_tags          TEXT NOT NULL DEFAULT '[]',
  metadata              TEXT NOT NULL DEFAULT '{}',
  request_body          TEXT,
  response_body         TEXT,
  session_id            TEXT,
  litellm_call_id       TEXT,
  created_at            BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_spend_logs_start      ON spend_logs(start_time);
CREATE INDEX IF NOT EXISTS idx_spend_logs_key_start  ON spend_logs(api_key, start_time);
CREATE INDEX IF NOT EXISTS idx_spend_logs_team_start ON spend_logs(team_id, start_time);
CREATE INDEX IF NOT EXISTS idx_spend_logs_user_start ON spend_logs(user_id, start_time);
CREATE INDEX IF NOT EXISTS idx_spend_logs_session    ON spend_logs(session_id);

CREATE TABLE IF NOT EXISTS daily_spend (
  day                   TEXT NOT NULL,
  api_key               TEXT NOT NULL DEFAULT '',
  model                 TEXT NOT NULL DEFAULT '',
  custom_llm_provider   TEXT NOT NULL DEFAULT '',
  model_group           TEXT NOT NULL DEFAULT '',
  team_id               TEXT NOT NULL DEFAULT '',
  user_id               TEXT NOT NULL DEFAULT '',
  prompt_tokens         BIGINT NOT NULL DEFAULT 0,
  completion_tokens     BIGINT NOT NULL DEFAULT 0,
  cache_read_input_tokens     BIGINT NOT NULL DEFAULT 0,
  cache_creation_input_tokens BIGINT NOT NULL DEFAULT 0,
  spend                 DOUBLE PRECISION NOT NULL DEFAULT 0,
  api_requests          BIGINT NOT NULL DEFAULT 0,
  successful_requests   BIGINT NOT NULL DEFAULT 0,
  failed_requests       BIGINT NOT NULL DEFAULT 0,
  updated_at            BIGINT NOT NULL,
  PRIMARY KEY (day, api_key, model, custom_llm_provider, model_group, team_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_daily_spend_day_team ON daily_spend(day, team_id);
CREATE INDEX IF NOT EXISTS idx_daily_spend_day_user ON daily_spend(day, user_id);

CREATE TABLE IF NOT EXISTS kv (
  k                     TEXT PRIMARY KEY,
  v                     TEXT NOT NULL,
  updated_at            BIGINT NOT NULL
);
