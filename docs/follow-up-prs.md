# Follow-Up PRs

Keep this list as the running queue for scoped follow-up PRs after the SQLite
repo-store slice.

1. Move last-run and run evidence from advisory JSON files into SQLite JSON
   rows, keeping the review surface unchanged.
2. Replace `repo.lock` with SQLite-backed transactions or short-lived leases so
   storage writes have one concurrency primitive.
3. Add a small runner boundary around the WinFsp virtual-mount backend before
   changing process orchestration.
4. Add the real workspace backend after the runner boundary exists and storage
   evidence is centralized.
5. Add targeted storage benchmarks and packaging checks once the backend shape
   settles.
