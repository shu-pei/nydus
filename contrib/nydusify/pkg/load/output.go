package load

import (
	"context"
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"

	"github.com/dragonflyoss/image-service/contrib/nydusify/pkg/parser"
	"github.com/dragonflyoss/image-service/contrib/nydusify/pkg/utils"
	"github.com/pkg/errors"
	"github.com/sirupsen/logrus"
)

func prettyDump(obj interface{}, name string) error {
	bytes, err := json.MarshalIndent(obj, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(name, bytes, 0644)
}

// Output outputs Nydus image nydus_bootstrap file and manifest, config to JSON file.
func (load *Load) Output(
	ctx context.Context, sourceParsed *parser.Parsed, outputPath string, index int,
) error {
	if sourceParsed.Index != nil {
		if err := prettyDump(
			sourceParsed.Index,
			filepath.Join(outputPath, "nydus_index.json"),
		); err != nil {
			return errors.Wrap(err, "output nydus index file")
		}
	}
	if sourceParsed.NydusImage != nil {
		if err := prettyDump(
			sourceParsed.NydusImage.Manifest,
			filepath.Join(outputPath, "nydus_manifest.json"),
		); err != nil {
			return errors.Wrap(err, "output Nydus manifest file")
		}
		if err := prettyDump(
			sourceParsed.NydusImage.Config,
			filepath.Join(outputPath, "nydus_config.json"),
		); err != nil {
			return errors.Wrap(err, "output Nydus config file")
		}

		// 拉取bootstrap
		source := filepath.Join(outputPath, "nydus_bootstrap")
		logrus.Infof("Pulling Nydus bootstrap to %s", source)
		bootstrapReader, err := load.sourcesParser[index].PullNydusBootstrap(ctx, sourceParsed.NydusImage)
		if err != nil {
			return errors.Wrap(err, "pull Nydus bootstrap layer")
		}
		defer bootstrapReader.Close()

		if err := utils.UnpackFile(bootstrapReader, utils.BootstrapFileNameInLayer, source); err != nil {
			return errors.Wrap(err, "unpack Nydus bootstrap layer")
		}

		//拉取blob
		blobPath := filepath.Join(outputPath, "blobs")
		if err := os.MkdirAll(blobPath, fs.ModePerm); err != nil {
			return errors.Wrap(err, "creat work directory")
		}
		logrus.Infof("Pulling Nydus blob to %s", blobPath)
		err = load.sourcesParser[index].PullNydusBlob(ctx, sourceParsed.NydusImage, blobPath)

	} else {
		err := fmt.Errorf("the %s is not a Nydus image", load.sourcesParser[index].Remote.Ref)
		return err
	}
	return nil
}
