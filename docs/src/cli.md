# CLI Reference

## Commands

### `doctor`

```bash
git-automate doctor
```

The doctor command helps setup or fix a project.
It does the following:

1. checks if a GITHUB_TOKEN env var is there, either already set or after loading .env file
1. Check if a git-automate.yml exists
2. If it doesnt exist:
   3. it checks if the local directory is in a git repository and sets up a default git-automate.yml that uses the git repository
   4. if it is not a git repository it will ask for a repository name like `krampenschiesser/git-automate`
4. Checks if the git repository has a project (user project first, then organization project), if it doesn't:
   5. Asks the user via stdin/stdout if it should use user projects or organization projects
   6. if organization is selected it will ask for the organization name
   7. creates a project with the name of the repository on either the user or organization and associates it with the git repository
8. Now that there is a project it ensures that it has the correct fields:
   9. the `Status` field is there and has the following options: `['Triage', 'Todo', 'InDevelopment', 'ReviewTechnical', 'ReviewProduct', 'QA', 'Done']`
   10. there is a text field for the `session_id`
11. It creates the full git-automate.yml file