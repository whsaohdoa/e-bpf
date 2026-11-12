import { useCallback, useEffect, useState } from 'react'

async function getJson(path) {
  const res = await fetch(path, { headers: { Accept: 'application/json' } })
  if (!res.ok) throw new Error(`${path} -> HTTP ${res.status}`)
  return res.json()
}

export default function App() {
  const [health, setHealth] = useState(null)
  const [machines, setMachines] = useState([])
  const [error, setError] = useState(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(async () => {
    setBusy(true)
    setError(null)
    try {
      const [h, m] = await Promise.all([getJson('/api/health'), getJson('/api/machines')])
      setHealth(h)
      setMachines(m)
    } catch (e) {
      setError(e.message)
    } finally {
      setBusy(false)
    }
  }, [])

  useEffect(() => {
    load()
  }, [load])

  return (
    <main>
      <header>
        <h1>Train 0 控制中心</h1>
        <p className="sub">管理面骨架 · 只读视图 · 认证与授权在 S1-2 落地</p>
        <button onClick={load} disabled={busy}>
          {busy ? '刷新中…' : '刷新'}
        </button>
      </header>

      {error && (
        <section className="error">
          <strong>请求失败</strong>
          <code>{error}</code>
          <span>后端未就绪或 /api 代理未生效。</span>
        </section>
      )}

      {health && (
        <section className="card">
          <h2>服务</h2>
          <dl>
            <dt>service</dt>
            <dd>{health.service}</dd>
            <dt>role</dt>
            <dd>{health.role}</dd>
            <dt>version</dt>
            <dd>{health.version}</dd>
            <dt>utc</dt>
            <dd>{health.utc}</dd>
          </dl>
        </section>
      )}

      <section className="card">
        <h2>机器 ({machines.length})</h2>
        <table>
          <thead>
            <tr>
              <th>id</th>
              <th>名称</th>
              <th>组</th>
              <th>状态</th>
              <th>最后心跳</th>
            </tr>
          </thead>
          <tbody>
            {machines.map((m) => (
              <tr key={m.id}>
                <td><code>{m.id}</code></td>
                <td>{m.name}</td>
                <td>
                  {m.group_name} <span className="muted">({m.group_id})</span>
                </td>
                <td><span className={`badge ${m.status}`}>{m.status}</span></td>
                <td className="muted">{m.last_seen ?? '—'}</td>
              </tr>
            ))}
            {machines.length === 0 && (
              <tr>
                <td colSpan="5" className="muted">没有数据</td>
              </tr>
            )}
          </tbody>
        </table>
      </section>
    </main>
  )
}
