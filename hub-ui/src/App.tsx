import { BrowserRouter, Link, Route, Routes } from "react-router-dom"

import { Layout } from "@/components/Layout"
import { SessionProvider } from "@/lib/session"
import { Home } from "@/pages/Home"
import { Login } from "@/pages/Login"
import { Register } from "@/pages/Register"
import { Orgs } from "@/pages/Orgs"
import { NewAgent } from "@/pages/NewAgent"
import { Agent } from "@/pages/Agent"
import { Settings } from "@/pages/Settings"
import { Session } from "@/pages/Session"
import { Diff } from "@/pages/Diff"
import { Tokens } from "@/pages/Tokens"
import { Audit } from "@/pages/Audit"

export default function App() {
  return (
    <BrowserRouter>
      <SessionProvider>
        <Layout>
          <Routes>
            <Route path="/" element={<Home />} />
            <Route path="/login" element={<Login />} />
            <Route path="/register" element={<Register />} />
            <Route path="/new" element={<NewAgent />} />
            <Route path="/orgs" element={<Orgs />} />
            <Route path="/tokens" element={<Tokens />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="/agent/:name" element={<Agent />} />
            <Route path="/agent/:name/settings" element={<Settings />} />
            <Route path="/agent/:name/session/:id" element={<Session />} />
            <Route path="/agent/:name/session/:id/diff" element={<Diff />} />
            <Route path="*" element={<NotFound />} />
          </Routes>
        </Layout>
      </SessionProvider>
    </BrowserRouter>
  )
}

function NotFound() {
  return (
    <div className="readout rounded-lg border px-6 py-12 text-center">
      <p className="eyebrow">404</p>
      <p className="mb-1 mt-2 font-semibold">No such page</p>
      <p className="mx-auto mb-5 max-w-[46ch] text-sm text-muted-foreground">
        Nothing is routed here. A private agent you can't read looks the same as one that
        doesn't exist — that's deliberate.
      </p>
      <Link to="/" className="font-mono text-sm text-primary hover:underline">
        ← all agents
      </Link>
    </div>
  )
}
