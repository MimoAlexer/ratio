#!/usr/bin/env python3
"""Create or reconcile the private Working Set board and its launch issues.

Run with Python 3 and a gh session authorized for repository and project access.
Use GH_BIN to select a particular gh installation. This script cannot grant scopes.
"""
import argparse
import json
import os
import subprocess
import sys

TITLE = "Working Set"
DESCRIPTION = "Desktop release qualification, account sync, provider staging, and the Working Set roadmap."
PROJECT_FIELDS = """
  id number title public url owner { ... on User { login } }
  repositories(first:100){nodes{nameWithOwner}}
"""
DETAILS = """
  views(first:100){nodes{id name number layout}}
  field(name:"Status"){
    ... on ProjectV2SingleSelectField {id options{id name}}
  }
"""
ITEM_FIELDS = """
  id isArchived
  content {... on Issue {id number repository{nameWithOwner}}}
  fieldValueByName(name:"Status") {... on ProjectV2ItemFieldSingleSelectValue {name}}
"""


def gh(*args, payload=None):
    command = [os.environ.get("GH_BIN", "gh"), "api", *args]
    if payload is not None:
        command.extend(["--input", "-"])
    result = subprocess.run(command, input=json.dumps(payload) if payload else None,
                            text=True, capture_output=True)
    if result.returncode:
        if "INSUFFICIENT_SCOPES" in result.stdout:
            raise RuntimeError("GitHub requires project access. Authorize gh with "
                               "'gh auth refresh -h github.com -s project', then retry.")
        raise RuntimeError(result.stderr.strip() or "GitHub API request failed")
    return json.loads(result.stdout)


def graph(query, **variables):
    return gh("graphql", payload={"query": query, "variables": variables})["data"]


def verify_project(board, owner, repository):
    linked = [node["nameWithOwner"] for node in board["repositories"]["nodes"]]
    if (board["public"] or board["title"] != TITLE or
            board["owner"].get("login") != owner or repository not in linked):
        raise ValueError("Board privacy, owner, title, or repository linkage failed verification.")


def board_items(project_id):
    items = {}
    cursor = None
    while True:
        query = """query($id:ID!, $after:String) {
          node(id:$id) {... on ProjectV2 {items(first:100, after:$after) {
            nodes{""" + ITEM_FIELDS + """} pageInfo{hasNextPage endCursor}
          }}}
        }"""
        page = graph(query, id=project_id, after=cursor)["node"]["items"]
        for item in page["nodes"]:
            content = item["content"] or {}
            if "number" in content:
                items[content["id"]] = item
        if not page["pageInfo"]["hasNextPage"]:
            return items
        cursor = page["pageInfo"]["endCursor"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", help="owner/repository, e.g. octocat/working-set")
    parser.add_argument("issues", nargs="+", type=int, help="Issue numbers to include and reconcile")
    args = parser.parse_args()
    owner, name = args.repository.split("/")
    launch_issues = list(dict.fromkeys(args.issues))
    initial = graph("""
      query($owner:String!, $name:String!) {
        viewer{id login}
        repository(owner:$owner, name:$name) {
          id isPrivate nameWithOwner projectsV2(first:100){
            pageInfo{hasNextPage} nodes{""" + PROJECT_FIELDS + """}
          }
        }
      }""", owner=owner, name=name)
    repo = initial["repository"]
    if not repo or not repo["isPrivate"] or initial["viewer"]["login"] != owner:
        raise ValueError("An authenticated owner and a private repository are required.")
    if repo["projectsV2"]["pageInfo"]["hasNextPage"]:
        raise ValueError("This launch helper requires fewer than 101 linked projects.")
    issues = []
    for number in launch_issues:
        issue = gh(f"repos/{args.repository}/issues/{number}")
        if "pull_request" in issue:
            raise ValueError(f"#{number} is a pull request.")
        issues.append(issue)
    matches = [p for p in repo["projectsV2"]["nodes"]
               if p["title"] == TITLE and p["owner"].get("login") == owner]
    if len(matches) > 1:
        raise ValueError("Multiple linked projects share the requested title.")
    if matches:
        project = matches[0]
    else:
        query = """mutation($owner:ID!, $repo:ID!, $title:String!) {
          createProjectV2(input:{ownerId:$owner,repositoryId:$repo,title:$title}) {
            projectV2{""" + PROJECT_FIELDS + """}
          }
        }"""
        project = graph(query, owner=initial["viewer"]["id"], repo=repo["id"],
                        title=TITLE)["createProjectV2"]["projectV2"]
    verify_project(project, owner, repo["nameWithOwner"])
    project_id = project["id"]
    graph("""mutation($id:ID!, $desc:String!){
      updateProjectV2(input:{projectId:$id,public:false,shortDescription:$desc}){
        projectV2{id public}
      }
    }""", id=project_id, desc=DESCRIPTION)
    data = graph("query($id:ID!){node(id:$id){... on ProjectV2{" + DETAILS + "}}}", id=project_id)["node"]
    status = data["field"]
    if not status:
        raise ValueError("The project requires a single-select Status field.")
    options = {o["name"].casefold(): o["id"] for o in status["options"]}
    if not {"todo", "done"} <= options.keys():
        raise ValueError("The Status field needs Todo and Done options.")
    views = [view for view in data["views"]["nodes"] if view["layout"] == "BOARD_LAYOUT"]
    if not views:
        created = graph("""mutation($id:ID!){
          createProjectV2View(input:{projectId:$id,name:"Delivery",layout:BOARD_LAYOUT}){
            projectV2View{number layout}
          }
        }""", id=project_id)
        views = [created["createProjectV2View"]["projectV2View"]]
    existing = board_items(project_id)
    for issue in issues:
        node_id = issue["node_id"]
        item = existing.get(node_id)
        if item and item["isArchived"]:
            raise ValueError(f"#{issue['number']} is archived on the board.")
        if not item:
            added = graph("""mutation($project:ID!, $issue:ID!){
              addProjectV2ItemById(input:{projectId:$project,contentId:$issue}){item{id}}
            }""", project=project_id, issue=node_id)
            item = added["addProjectV2ItemById"]["item"]
        current = (item.get("fieldValueByName") or {}).get("name", "").casefold()
        desired = "done" if issue["state"] == "closed" else "todo"
        if current == desired or (desired == "todo" and current not in {"", "done"}):
            continue
        graph("""mutation($project:ID!, $item:ID!, $field:ID!, $option:String!){
          updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,
              value:{singleSelectOptionId:$option}}){projectV2Item{id}}
        }""", project=project_id, item=item["id"], field=status["id"], option=options[desired])

    final = graph("query($id:ID!){node(id:$id){... on ProjectV2{" +
                  PROJECT_FIELDS + " shortDescription views(first:100){nodes{number layout}}}}}", id=project_id)["node"]
    verify_project(final, owner, repo["nameWithOwner"])
    if final["shortDescription"] != DESCRIPTION:
        raise ValueError("Project description failed verification.")
    if not any(v["layout"] == "BOARD_LAYOUT" and v["number"] == views[0]["number"]
               for v in final["views"]["nodes"]):
        raise ValueError("Board layout failed verification.")
    refreshed = board_items(project_id)
    for issue in issues:
        item = refreshed.get(issue["node_id"])
        state = ((item or {}).get("fieldValueByName") or {}).get("name", "")
        if not state or item["isArchived"] or (state.casefold() == "done") != (issue["state"] == "closed"):
            raise ValueError(f"Membership or status verification failed for #{issue['number']}.")
        print(f"#{issue['number']} {state}")
    print(f"Private board: {final['url']}/views/{views[0]['number']}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError) as error:
        sys.exit(str(error))
