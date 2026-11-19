# Hosted Grafana dashboards

Dashboards for the hosted Fynd deployments, in Grafana's `fynd` folder. They query the shared
VictoriaMetrics datasource and filter on the `namespace` (`dev-fynd`, `staging-fynd`, `prod-fynd`),
`chain` and `container` labels of the Kubernetes scrape, so they do not work with the local
`docker-compose.yml` stack. The local stack's dashboard is in `../dashboards/`.

The `sync-grafana-dashboards` job in `.github/workflows/v2-main-workflow.yaml` pushes every file in
this directory that a push to `main` adds or changes. It uses the `GRAFANA_SA_TOKEN` secret.

These files are the source of truth: an edit made in the Grafana UI is overwritten the next time its
file changes. To change a dashboard, edit it in Grafana, export it (*Export → Export as JSON*), and
commit the result over the file. Keep the `uid` unchanged. Deleting a file does not delete the
dashboard.

| File | Dashboard |
|------|-----------|
| `fynd-pamm-simulation-overrides.json` | Whether pAMM swaps in served quotes carry Titan's simulation overrides, how routes merge them, how simulations on them land (`base_block`, deviation), and the health of Titan's quote stream |
