const TOKEN_KEY = 'rustllmroute.token'

export class ApiError extends Error {
  status: number
  body: unknown

  constructor(status: number, message: string, body: unknown) {
    super(message)
    this.status = status
    this.body = body
  }
}

export function getToken(): string | null {
  try {
    return sessionStorage.getItem(TOKEN_KEY)
  } catch {
    return null
  }
}

export function setToken(token: string) {
  try {
    sessionStorage.setItem(TOKEN_KEY, token)
  } catch {
    // storage unavailable: the session cookie still works
  }
}

export function clearToken() {
  try {
    sessionStorage.removeItem(TOKEN_KEY)
  } catch {
    // ignore
  }
}

function messageOf(body: unknown, fallback: string): string {
  if (body && typeof body === 'object') {
    const record = body as Record<string, unknown>
    const error = record.error
    if (error && typeof error === 'object') {
      const message = (error as Record<string, unknown>).message
      if (typeof message === 'string') return message
    }
    if (typeof record.message === 'string') return record.message
    if (typeof record.detail === 'string') return record.detail
  }
  return fallback
}

export interface RequestOptions {
  method?: string
  json?: unknown
  query?: Record<string, string | number | boolean | undefined | null>
  redirectOnUnauthorized?: boolean
}

export async function api<T = unknown>(path: string, options: RequestOptions = {}): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (options.json !== undefined) headers['Content-Type'] = 'application/json'
  const token = getToken()
  if (token) headers.Authorization = `Bearer ${token}`
  let url = path
  if (options.query) {
    const params = new URLSearchParams()
    for (const [key, value] of Object.entries(options.query)) {
      if (value !== undefined && value !== null && value !== '') params.set(key, String(value))
    }
    const encoded = params.toString()
    if (encoded) url += (url.includes('?') ? '&' : '?') + encoded
  }
  const response = await fetch(url, {
    method: options.method ?? (options.json !== undefined ? 'POST' : 'GET'),
    headers,
    credentials: 'include',
    body: options.json !== undefined ? JSON.stringify(options.json) : undefined,
  })
  const contentType = response.headers.get('content-type') ?? ''
  const body: unknown = contentType.includes('application/json')
    ? await response.json().catch(() => null)
    : await response.text()
  if (response.status === 401 && options.redirectOnUnauthorized !== false) {
    clearToken()
    if (!window.location.pathname.endsWith('/login')) {
      window.location.assign('/ui/login')
    }
  }
  if (!response.ok) {
    throw new ApiError(response.status, messageOf(body, `${response.status} ${response.statusText}`), body)
  }
  return body as T
}

export function formatUsd(value: number | null | undefined, digits = 4): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '—'
  return `$${value.toFixed(digits)}`
}

export function formatNumber(value: number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  return new Intl.NumberFormat().format(value)
}

export function formatEpochMs(value: number | string | null | undefined): string {
  if (value === null || value === undefined || value === '') return '—'
  const date = typeof value === 'number' ? new Date(value) : new Date(value)
  if (Number.isNaN(date.getTime())) return String(value)
  return date.toLocaleString()
}

export function isoDate(date: Date): string {
  return date.toISOString().slice(0, 10)
}
