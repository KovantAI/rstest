| Engine | When | Granularity |
|---|---|---|
| **Import graph** | always available, zero setup | whole test *files* that transitively import a changed module |
| **Coverage index** | when a line→test index is warm | individual *tests* whose recorded coverage hit the changed *lines* |
