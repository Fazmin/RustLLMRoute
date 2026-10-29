import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { api, formatNumber } from '../api'
import { Badge, Button, ErrorNote, Loading, PageTitle, Table } from '../components'
import type { HealthReport, ModelGroup, ModelInfoEntry } from '../types'

function healthBadge(entry: ModelInfoEntry) {
  const health = entry.model_info.health
  if (health?.cooldown_remaining_s !== undefined && health?.cooldown_remaining_s !== null) {
    return <Badge tone="warn">cooling {health.cooldown_remaining_s}s</Badge>
  }
  switch (health?.status) {
    case 'healthy':
      return <Badge tone="good">healthy{health.latency_ms !== undefined ? ` · ${health.latency_ms} ms` : ''}</Badge>
    case 'unhealthy':
      return (
        <Badge tone="bad">
          unhealthy
        </Badge>
      )
    default:
      return <Badge>unchecked</Badge>
  }
}

export function ModelsPage() {
  const queryClient = useQueryClient()
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const groups = useQuery({ queryKey: ['model-groups'], queryFn: () => api<{ data: ModelGroup[] }>('/model_group/info') })
  const models = useQuery({ queryKey: ['model-info'], queryFn: () => api<{ data: ModelInfoEntry[] }>('/model/info') })

  async function runHealthCheck() {
    setChecking(true)
    setError(null)
    try {
      await api<HealthReport>('/health')
      await queryClient.invalidateQueries({ queryKey: ['model-info'] })
      await queryClient.invalidateQueries({ queryKey: ['model-groups'] })
    } catch (failure) {
      setError(failure)
    } finally {
      setChecking(false)
    }
  }

  return (
    <div>
      <PageTitle
        title="Models"
        actions={
          <Button onClick={runHealthCheck} disabled={checking}>
            {checking ? 'Checking…' : 'Run health check'}
          </Button>
        }
      />
      <ErrorNote error={error ?? groups.error ?? models.error} />
      <div className="mb-8 grid grid-cols-1 gap-4 md:grid-cols-2 xl:grid-cols-3">
        {(groups.data?.data ?? []).map((group) => (
          <div key={group.model_group} className="rounded-lg border border-slate-200 bg-white p-4">
            <div className="flex items-center justify-between">
              <div className="font-semibold">{group.model_group}</div>
              <Badge tone={group.cooled_down > 0 ? 'warn' : 'neutral'}>
                {group.deployments} deployment{group.deployments === 1 ? '' : 's'}
              </Badge>
            </div>
            <div className="mt-2 text-sm text-slate-600">{group.providers.join(', ')} · {group.mode}</div>
            <div className="mt-2 text-xs text-slate-500">
              context {group.max_input_tokens ? formatNumber(group.max_input_tokens) : '?'} · output {group.max_output_tokens ? formatNumber(group.max_output_tokens) : '?'}
              <br />
              in {group.input_cost_per_token !== null ? `$${(group.input_cost_per_token * 1e6).toFixed(2)}/M` : '?'} · out{' '}
              {group.output_cost_per_token !== null ? `$${(group.output_cost_per_token * 1e6).toFixed(2)}/M` : '?'}
            </div>
            <div className="mt-2 flex gap-1 text-xs">
              {group.supports_messages_api ? <Badge>messages native</Badge> : null}
              {group.supports_responses_api ? <Badge>responses native</Badge> : null}
            </div>
          </div>
        ))}
      </div>
      {models.isLoading ? (
        <Loading />
      ) : (
        <Table
          rows={models.data?.data ?? []}
          rowKey={(row) => row.model_info.id}
          columns={[
            { key: 'name', header: 'Group', render: (row) => row.model_name },
            { key: 'id', header: 'Deployment', render: (row) => <span className="font-mono text-xs">{row.model_info.id}</span> },
            { key: 'model', header: 'Provider model', render: (row) => row.litellm_params.model },
            { key: 'base', header: 'API base', render: (row) => <span className="text-xs text-slate-500">{row.litellm_params.api_base}</span> },
            { key: 'limits', header: 'Limits', render: (row) => `${row.litellm_params.rpm ?? '∞'} rpm · ${row.litellm_params.tpm ?? '∞'} tpm` },
            { key: 'health', header: 'Health', render: healthBadge },
            { key: 'error', header: 'Last error', render: (row) => <span className="text-xs text-red-700">{row.model_info.health?.error ?? ''}</span> },
          ]}
        />
      )}
    </div>
  )
}
