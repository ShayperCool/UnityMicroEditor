package main

import (
	"context"
	"os"
	"testing"
	"time"

	"unitymicroeditor/worker/internal/editorbridge"
)

func TestAgentHostPreparationActualEditor(t *testing.T) {
	project, editor := os.Getenv("UNITY_AGENT_HOST_STAGE_PROJECT"), os.Getenv("UNITY_AGENT_HOST_STAGE_EDITOR")
	if project == "" || editor == "" {
		t.Skip("set explicit disposable target and matching Editor to verify isolated automatic host preparation")
	}
	id := os.Getenv("UNITY_AGENT_HOST_STAGE_ID")
	if id == "" {
		id = "actual-isolated-host01"
	}
	ctx, cancel := context.WithTimeout(context.Background(), 12*time.Minute)
	defer cancel()
	source, err := editorbridge.New(ctx, editorbridge.Options{Project: project, Editor: editor, StartupTimeout: 10 * time.Minute})
	if err != nil {
		t.Fatal(err)
	}
	connection, err := source.OwnProcessOrAttach(ctx)
	if err != nil {
		source.Close()
		t.Fatal(err)
	}
	service := &agentProjectService{project: project, editor: editor, client: source, lifetime: ctx}
	defer service.Close()
	host, evidence, err := prepareAgentHost(ctx, service, id)
	if err != nil {
		t.Fatal(err)
	}
	if evidence["backend"] != "unity_editor" || evidence["sourceSnapshotVerified"] != true || evidence["warmLibraryFromOwnedStoppedEditor"] != connection.Owned {
		t.Fatalf("isolated host lost backend/source/ownership proof: %+v", evidence)
	}
	if connection.Owned && service.client != nil {
		t.Fatal("stopped owned source client was retained")
	}
	replayed, _, err := prepareAgentHost(ctx, service, id)
	if err != nil || replayed != host {
		t.Fatalf("confirmed host replay changed output: %s %v", replayed, err)
	}
	t.Logf("actual isolated host=%s stage=%s receipt=%s sourceSnapshot=%s warmOwnedSeed=%v", host, evidence["stageProject"], evidence["receipt"], evidence["sourceSnapshot"], evidence["warmLibraryFromOwnedStoppedEditor"])
}
