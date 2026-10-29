import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { api } from '../api'
import { Button, ErrorNote, JsonView, Loading, PageTitle } from '../components'
import type { Readiness } from '../types'

interface ConfigView {
  path: string
  yaml: string
  loaded_at: number
  models: number
  guardrails: string[]
}

export function SettingsPage() {
  const queryClient = useQueryClient()
  const [reloadResult, setReloadResult] = useState<unknown>(null)
  const [error, setError] = useState<unknown>(null)
  const [busy, setBusy] = useState(false)
  const config = useQuery({ queryKey: ['config'], queryFn: () => api<ConfigView>('/config/yaml') })
  const readiness = useQuery({ queryKey: ['readiness'], queryFn: () => api<Readiness>('/health/readiness') })

  async function reload() {
    setBusy(true)
    setError(null)
    try {
      setReloadResult(await api('/config/reload', { method: 'POST', json: {} }))
      await queryClient.invalidateQueries()
    } catch (failure) {
      setError(failure)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div>
      <PageTitle
        title="Settings"
        actions={
          <Button onClick={reload} disabled={busy}>
            {busy ? 'Reloading…' : 'Reload config'}
          </Button>
        }
      />
      <ErrorNote error={error ?? config.error} />
      {reloadResult ? (
        <div className="mb-4">
          <div className="mb-1 text-sm font-medium">Reload result</div>
          <JsonView value={reloadResult} />
        </div>
      ) : null}
      <div className="grid grid-cols-1 gap-6 lg:grid-cols-2">
        <div>
          <h2 className="mb-2 text-lg font-semibold">Configuration</h2>
          {config.isLoading ? (
            <Loading />
          ) : config.data ? (
            <>
              <p className="mb-2 text-xs text-slate-500">
                {config.data.path} · {config.data.models} deployments · loaded {new Date(config.data.loaded_at * 1000).toLocaleString()}
                {config.data.guardrails.length ? ` · guardrails: ${config.data.guardrails.join(', ')}` : ''}
              </p>
              <pre className="max-h-[32rem] overflow-auto rounded bg-slate-900 p-3 text-xs text-slate-100">{config.data.yaml}</pre>
            </>
          ) : null}
        </div>
        <div>
          <h2 className="mb-2 text-lg font-semibold">Readiness</h2>
          {readiness.data ? <JsonView value={readiness.data} /> : <Loading />}
          <h2 className="mt-6 mb-2 text-lg font-semibold">Endpoints</h2>
          <ul className="list-disc space-y-1 pl-5 text-sm text-slate-700">
            <li>
              <a className="underline" href="/metrics" target="_blank" rel="noreferrer">
                /metrics
              </a>{' '}
              Prometheus exposition
            </li>
            <li>
              <a className="underline" href="/health/readiness" target="_blank" rel="noreferrer">
                /health/readiness
              </a>
            </li>
            <li>
              <code>/v1/chat/completions</code>, <code>/v1/messages</code>, <code>/v1/responses</code>, <code>/v1/embeddings</code>
            </li>
          </ul>
        </div>
      </div>
    </div>
  )
}
