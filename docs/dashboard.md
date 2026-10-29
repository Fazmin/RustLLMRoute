# Admin dashboard

The dashboard is a Vite + React single-page app served by the gateway at `/ui/`. The Docker
image builds it in; for a local binary run `npm --prefix ui install && npm --prefix ui run build`
before `cargo build` (release builds embed `ui/dist`; debug builds read it from disk). Without a
build the gateway serves a placeholder page that says so.

## Signing in

`POST /login` with `{"username", "password"}` checks `UI_USERNAME` (default `admin`) and
`UI_PASSWORD` (default: the master key). It returns a 24-hour HS256 token signed with the master
key and sets it as the `rustllmroute_ui` cookie (`HttpOnly`, `SameSite=Lax`; add `Secure` with
`general_settings.ui_secure_cookie: true`). The same token works as `Authorization: Bearer` for
scripts. Failed logins are limited to ten per minute per gateway. `POST /logout` clears the
cookie. The login page also accepts a master key pasted directly; it is kept in session storage
for that browser tab only.

## Pages

| Page | Backing routes |
|---|---|
| Overview | `/health/readiness`, `/model/info`, `/user/daily/activity`, `/global/spend`, `/spend/logs/ui` |
| Keys | `/key/list`, `/key/generate`, `/key/block`, `/key/unblock`, `/key/{key}/regenerate`, `/key/delete` |
| Teams | `/team/list`, `/team/new`, `/team/block`, `/team/unblock`, `/team/delete` |
| Users | `/user/list`, `/user/new`, `/user/delete` |
| Models | `/model_group/info`, `/model/info`, `/health` (runs probes on demand) |
| Logs | `/spend/logs/ui` with key, model, status and date filters |
| Usage | `/user/daily/activity` grouped by model, key, team, user or provider |
| Settings | `/config/yaml` (secrets redacted), `/config/reload`, `/health/readiness` |

Keys, teams, users, logs and usage need a database (`DATABASE_URL`); the other pages work in
master-key-only mode.

## Config reload

`POST /config/reload` re-reads the config file. `model_list` and `model_group_alias` take effect
immediately; `router_settings`, `guardrails` and `general_settings` changes are reported under
`requires_restart`. An invalid file is rejected with 400 and the running configuration stays live.

## Development

`npm --prefix ui run dev` serves the app on Vite's dev server with API calls proxied to
`http://localhost:4000`; sign in with the master key.
