import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, formatEpochMs, formatUsd } from '../api'
import { Badge, Button, ErrorNote, Field, Loading, Modal, PageTitle, Pager, Table, numberOrUndefined, splitList } from '../components'
import type { KeyRow } from '../types'

interface KeyList {
  keys: KeyRow[]
  total_count: number
  current_page: number
  total_pages: number
}

export function KeysPage() {
  const queryClient = useQueryClient()
  const [page, setPage] = useState(1)
  const [showCreate, setShowCreate] = useState(false)
  const [revealed, setRevealed] = useState<{ key: string; alias: string | null } | null>(null)
  const [error, setError] = useState<unknown>(null)
  const list = useQuery({ queryKey: ['keys', page], queryFn: () => api<KeyList>('/key/list', { query: { page, size: 50, include_blocked: true } }) })

  const refresh = () => queryClient.invalidateQueries({ queryKey: ['keys'] })
  const action = useMutation({
    mutationFn: async ({ path, body }: { path: string; body: unknown }) => api<KeyRow & { key?: string }>(path, { json: body }),
    onSuccess: (result) => {
      refresh()
      if (result?.key) setRevealed({ key: result.key, alias: result.key_alias })
    },
    onError: setError,
  })

  const columns = [
    { key: 'name', header: 'Key', render: (row: KeyRow) => <span className="font-mono text-xs">{row.key_name ?? row.token.slice(0, 12)}</span> },
    { key: 'alias', header: 'Alias', render: (row: KeyRow) => row.key_alias ?? '—' },
    { key: 'spend', header: 'Spend', render: (row: KeyRow) => `${formatUsd(row.spend)} / ${row.max_budget === null ? '∞' : formatUsd(row.max_budget, 2)}` },
    { key: 'models', header: 'Models', render: (row: KeyRow) => (row.models.length ? row.models.join(', ') : 'all') },
    { key: 'team', header: 'Team', render: (row: KeyRow) => row.team_id ?? '—' },
    { key: 'limits', header: 'Limits', render: (row: KeyRow) => `${row.rpm_limit ?? '∞'} rpm · ${row.tpm_limit ?? '∞'} tpm` },
    { key: 'expires', header: 'Expires', render: (row: KeyRow) => (row.expires ? formatEpochMs(row.expires) : 'never') },
    { key: 'status', header: 'Status', render: (row: KeyRow) => (row.blocked ? <Badge tone="bad">blocked</Badge> : <Badge tone="good">active</Badge>) },
    {
      key: 'actions',
      header: '',
      render: (row: KeyRow) => (
        <div className="flex gap-1">
          <Button kind="secondary" onClick={() => action.mutate({ path: row.blocked ? '/key/unblock' : '/key/block', body: { key: row.token } })}>
            {row.blocked ? 'Unblock' : 'Block'}
          </Button>
          <Button kind="secondary" onClick={() => action.mutate({ path: `/key/${row.token}/regenerate`, body: {} })}>
            Regenerate
          </Button>
          <Button
            kind="danger"
            onClick={() => {
              if (window.confirm(`Delete key ${row.key_alias ?? row.key_name ?? row.token}?`)) action.mutate({ path: '/key/delete', body: { keys: [row.token] } })
            }}
          >
            Delete
          </Button>
        </div>
      ),
    },
  ]

  return (
    <div>
      <PageTitle title="Virtual keys" actions={<Button onClick={() => setShowCreate(true)}>Create key</Button>} />
      <ErrorNote error={error ?? list.error} />
      {list.isLoading ? <Loading /> : <Table rows={list.data?.keys ?? []} rowKey={(row) => row.token} columns={columns} empty="No keys yet. Create one to get started." />}
      <Pager page={list.data?.current_page ?? 1} totalPages={list.data?.total_pages ?? 1} onChange={setPage} />
      {showCreate ? (
        <CreateKeyModal
          onClose={() => setShowCreate(false)}
          onCreate={(body) => {
            setShowCreate(false)
            action.mutate({ path: '/key/generate', body })
          }}
        />
      ) : null}
      {revealed ? (
        <Modal title="Key created" onClose={() => setRevealed(null)}>
          <p className="mb-2 text-sm text-slate-600">Copy this key now; it is not stored and cannot be shown again.</p>
          <pre className="rounded bg-slate-900 p-3 text-sm text-emerald-200 break-all whitespace-pre-wrap">{revealed.key}</pre>
          <div className="mt-3 flex gap-2">
            <Button onClick={() => navigator.clipboard?.writeText(revealed.key)}>Copy</Button>
            <Button kind="secondary" onClick={() => setRevealed(null)}>
              Done
            </Button>
          </div>
        </Modal>
      ) : null}
    </div>
  )
}

function CreateKeyModal({ onClose, onCreate }: { onClose: () => void; onCreate: (body: Record<string, unknown>) => void }) {
  const [alias, setAlias] = useState('')
  const [models, setModels] = useState('')
  const [maxBudget, setMaxBudget] = useState('')
  const [duration, setDuration] = useState('')
  const [budgetDuration, setBudgetDuration] = useState('')
  const [rpm, setRpm] = useState('')
  const [tpm, setTpm] = useState('')
  const [team, setTeam] = useState('')
  const [user, setUser] = useState('')
  return (
    <Modal title="Create a virtual key" onClose={onClose}>
      <div className="grid grid-cols-2 gap-3">
        <Field label="Alias" value={alias} onChange={setAlias} placeholder="my-agent" />
        <Field label="Models" value={models} onChange={setModels} placeholder="gpt-4o, claude" hint="Comma separated; empty = all" />
        <Field label="Max budget (USD)" value={maxBudget} onChange={setMaxBudget} placeholder="10" />
        <Field label="Budget resets every" value={budgetDuration} onChange={setBudgetDuration} placeholder="30d" />
        <Field label="Expires after" value={duration} onChange={setDuration} placeholder="90d" />
        <Field label="RPM limit" value={rpm} onChange={setRpm} />
        <Field label="TPM limit" value={tpm} onChange={setTpm} />
        <Field label="Team id" value={team} onChange={setTeam} />
        <Field label="User id" value={user} onChange={setUser} />
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button kind="secondary" onClick={onClose}>
          Cancel
        </Button>
        <Button
          onClick={() =>
            onCreate({
              key_alias: alias || undefined,
              models: splitList(models),
              max_budget: numberOrUndefined(maxBudget),
              budget_duration: budgetDuration || undefined,
              duration: duration || undefined,
              rpm_limit: numberOrUndefined(rpm),
              tpm_limit: numberOrUndefined(tpm),
              team_id: team || undefined,
              user_id: user || undefined,
            })
          }
        >
          Create
        </Button>
      </div>
    </Modal>
  )
}
