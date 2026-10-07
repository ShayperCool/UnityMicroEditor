package goproject

import "fmt"

const assetReferenceClosureMaximum = 32768

type assetReference struct {
	guid, fileID string
}

// Every producer shares this worklist. The bound counts unique identities over
// the whole traversal, including processed entries, rather than duplicate PPtrs.
type assetReferenceWorklist struct {
	items []assetReference
	seen  map[string]bool
}

func newAssetReferenceWorklist() *assetReferenceWorklist {
	return &assetReferenceWorklist{seen: map[string]bool{}}
}

func (work *assetReferenceWorklist) addIdentity(guid, id string) error {
	if guid == "" || builtinGUID(guid) || id == "" || id == "0" {
		return nil
	}
	key := assetKey(guid, id)
	if work.seen[key] {
		return nil
	}
	if len(work.seen) >= assetReferenceClosureMaximum {
		return fmt.Errorf("asset dependency closure exceeds supported unique-reference bound (%d)", assetReferenceClosureMaximum)
	}
	work.seen[key] = true
	work.items = append(work.items, assetReference{guid, id})
	return nil
}

func (work *assetReferenceWorklist) add(reference map[string]any) error {
	return work.addIdentity(str(reference["guid"]), fileID(reference))
}

func (work *assetReferenceWorklist) addAll(references []map[string]any) error {
	for _, reference := range references {
		if err := work.add(reference); err != nil {
			return err
		}
	}
	return nil
}

func (work *assetReferenceWorklist) pop() (assetReference, bool) {
	if len(work.items) == 0 {
		return assetReference{}, false
	}
	last := len(work.items) - 1
	reference := work.items[last]
	work.items[last] = assetReference{}
	work.items = work.items[:last]
	return reference, true
}
