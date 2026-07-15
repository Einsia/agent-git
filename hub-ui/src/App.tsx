import { BrowserRouter, Route, Routes } from "react-router-dom"

import { Layout } from "@/components/Layout"
import { Home } from "@/pages/Home"
import { Agent } from "@/pages/Agent"
import { Session } from "@/pages/Session"
import { Diff } from "@/pages/Diff"

export default function App() {
  return (
    <BrowserRouter>
      <Layout>
        <Routes>
          <Route path="/" element={<Home />} />
          <Route path="/agent/:name" element={<Agent />} />
          <Route path="/agent/:name/session/:id" element={<Session />} />
          <Route path="/agent/:name/session/:id/diff" element={<Diff />} />
          <Route path="*" element={<p className="text-muted-foreground">没找到。</p>} />
        </Routes>
      </Layout>
    </BrowserRouter>
  )
}
