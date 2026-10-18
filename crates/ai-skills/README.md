# Local Agent Skills registry

`SkillRegistry` is a local, single-writer catalog. The native host owns it on its
worker and supplies the engine capability allowlist. This crate has no network,
provider credentials, PDF analysis service, shell or script execution.

New folder/ZIP imports, complete edited packages and generated packages enter as
drafts. Identical content already in the catalog is idempotent and returns its
existing record. A new draft never changes the active approved revision. Human
approval requires a nonempty native sample, exact executed sample actions, all
declared native capabilities, a decoded bounded PNG and reviewer notes. The host
must execute on a scratch document, validate command parameters and actually show
that sample to the reviewer before calling `approve`. The registry does not claim
to infer human review or successful engine execution from pixels.

Packages contain `SKILL.md` with Agent Skills YAML frontmatter (`name` and
`description`), optional `references/` and `assets/`, native JSON `templates/`, and
a DesignCraft adapter manifest. Upstream packages without the adapter can be
cataloged as drafts. Upstream `allowed-tools` and external tools block approval
until adapted; no executable files or scripts are imported.

Example `designcraft.json`:

```json
{
  "schema_version": 1,
  "skill_id": "example-layout",
  "required_commands": ["frame.create"],
  "references": ["references/review.md"],
  "templates": ["templates/sample.json"],
  "sample_template": "templates/sample.json",
  "external_tools": [],
  "compatibility_notes": "Requires a scratch A4 page and visual review."
}
```

Example template:

```json
[
  {"command":"frame.create","params":{"rect":[36,36,559,100],"content":"text","text":"Sample","caret":false}}
]
```

Templates contain concrete native commands; the registry does not interpolate or
execute placeholders. Host-generated changes create a complete new draft package.
Imported ZIPs are read into bounded memory without extracting untrusted entries.
Limits are 256 files, 16 MiB per file, 32 MiB per package, 8 path components and
4096 actions per template. Traversal, absolute paths, symbolic links, Windows
reparse points/device names, duplicates and case collisions are rejected.

SHA-256 identifies every file and the complete revision. File loads and exports
verify their stored hashes. Snapshots own their selected instructions, textual
references and native template actions, so a later edit, approval or rollback
cannot change an in-flight task. Binary references and source PDFs are not sent
implicitly. Analysis cache entries use source hash plus analyzer version; the
caller can reuse analysis without repeatedly sending the source document.

Approved revisions stay immutable. Rollback selects a previously approved
revision; disabling prevents new snapshots while preserving pinned running
tasks. Four original early-years/school-print packages are installed as drafts
with review references and native samples. No original source PDF or secret is
included in these builtins.
