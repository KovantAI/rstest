# The warm source: the latest successful push run of this workflow on
# main. Matched by workflow file, not display name (two workflows can
# share a `name:`). No such run yet means an empty id: a cold start.
- id: warm
  env:
    GH_TOKEN: ${{ github.token }}
  shell: bash  # bash syntax: Windows runners default to PowerShell
  run: |
    wf="${GITHUB_WORKFLOW_REF##*/.github/workflows/}"; wf="${wf%%@*}"
    rid=$(gh run list --repo "$GITHUB_REPOSITORY" --workflow "$wf" \
            --branch main --event push --status success --limit 1 \
            --json databaseId --jq '.[0].databaseId // ""')
    echo "run-id=$rid" >> "$GITHUB_OUTPUT"
  continue-on-error: true
