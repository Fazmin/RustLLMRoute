import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, formatUsd } from '../api'
import { Badge, Button, ErrorNote, Field, Loading, Modal, PageTitle, Table, numberOrUndefined, splitList } from '../components'
import type { TeamRow } from '../types'

export function TeamsPage() {
  const queryClient = useQueryClient()
  const [showCreate, setShowCreate] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const list = useQuery({ queryKey: ['teams'], queryFn: () => api<{ teams: TeamRow[] }>('/team/list', { query: { size: 200 } }) })
  const action = useMutation({
    mutationFn: ({ path, body }: { path: string; body: unknown }) => api(path, { json: body }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['teams'] }),
    onError: setError,
  })
  const [alias, setAlias] = useState('')
  const [models, setModels] = useState('')
  const [maxBudget, setMaxBudget] = useState('')

  return (
    <div>
      <PageTitle title="Teams" actions={<Button onClick={() => setShowCreate(true)}>Create team</Button>} />
      <ErrorNote error={error ?? list.error} />
      {list.isLoading ? (
        <Loading />
      ) : (
        <Table
          rows={list.data?.teams ?? []}
          rowKey={(row) => row.team_id}
          columns={[
            { key: 'id', header: 'Team', render: (row) => <span className="font-mono text-xs">{row.team_id}</span> },
            { key: 'alias', header: 'Alias', render: (row) => row.team_alias ?? '—' },
            { key: 'spend', header: 'Spend', render: (row) => `${formatUsd(row.spend)} / ${row.max_budget === null ? '∞' : formatUsd(row.max_budget, 2)}` },
            { key: 'models', header: 'Models', render: (row) => (row.models.length ? row.models.join(', ') : 'all') },
            { key: 'members', header: 'Members', render: (row) => row.members_with_roles?.length ?? 0 },
            { key: 'status', header: 'Status', render: (row) => (row.blocked ? <Badge tone="bad">blocked</Badge> : <Badge tone="good">active</Badge>) },
            {
              key: 'actions',
              header: '',
              render: (row) => (
                <div className="flex gap-1">
                  <Button kind="secondary" onClick={() => action.mutate({ path: row.blocked ? '/team/unblock' : '/team/block', body: { team_id: row.team_id } })}>
                    {row.blocked ? 'Unblock' : 'Block'}
                  </Button>
                  <Button
                    kind="danger"
                    onClick={() => {
                      if (window.confirm(`Delete team ${row.team_alias ?? row.team_id}?`)) action.mutate({ path: '/team/delete', body: { team_ids: [row.team_id] } })
                    }}
                  >
                    Delete
                  </Button>
                </div>
              ),
            },
          ]}
          empty="No teams yet."
        />
      )}
      {showCreate ? (
        <Modal title="Create a team" onClose={() => setShowCreate(false)}>
          <div className="space-y-3">
            <Field label="Alias" value={alias} onChange={setAlias} placeholder="platform" />
            <Field label="Models" value={models} onChange={setModels} placeholder="gpt-4o, claude" hint="Comma separated; empty = all" />
            <Field label="Max budget (USD)" value={maxBudget} onChange={setMaxBudget} />
          </div>
          <div className="mt-4 flex justify-end gap-2">
            <Button kind="secondary" onClick={() => setShowCreate(false)}>
              Cancel
            </Button>
            <Button
              onClick={() => {
                setShowCreate(false)
                action.mutate({ path: '/team/new', body: { team_alias: alias || undefined, models: splitList(models), max_budget: numberOrUndefined(maxBudget) } })
              }}
            >
              Create
            </Button>
          </div>
        </Modal>
      ) : null}
    </div>
  )
}
