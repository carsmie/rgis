"""Print the labels QGIS places for a map, one JSON object per line (QgsLabelingResults).

Run with PyQGIS from the QGIS Server image (offscreen), e.g.:
  docker run --rm -e QT_QPA_PLATFORM=offscreen -v $OUT:$OUT:ro -v $PWD/tools/topo:/t:ro \\
    --entrypoint python3 tnt-qgis:fork-test /t/qgis_labels.py PROJECT LAYER xmin,ymin,xmax,ymax [SIZE]

Renders like a WMS GetMap: EPSG:25833, SIZE×SIZE px, OGC DPI (90.714), LAYER = a layer or
group name of the project, compared against rGIS's `rgis::labels` debug log.
"""
import json
import os
import sys

from qgis.core import (Qgis, QgsApplication, QgsCoordinateReferenceSystem, QgsLayerTreeGroup, QgsMapRendererSequentialJob,
                       QgsMapSettings, QgsProject, QgsRectangle)
from qgis.PyQt.QtCore import QSize

app = QgsApplication([], False)
app.initQgis()
project_path, name, bbox = sys.argv[1:4]
size = int(sys.argv[4]) if len(sys.argv) > 4 else 512
subset = sys.argv[5] if len(sys.argv) > 5 else ""  # like WMS FILTER: an expression on LAYER
project = QgsProject.instance()
project.read(project_path)

root = project.layerTreeRoot()
node = next((n for n in root.findLayers() if n.name() == name), None) or root.findGroup(name)
layers = [l.layer() for l in node.findLayers()] if isinstance(node, QgsLayerTreeGroup) else [node.layer()]
if subset:
    for l in layers:  # AND-ed with the layer's own subset, like WMS FILTER
        own = l.subsetString()
        l.setSubsetString(f"({own}) AND ({subset})" if own else subset)
# Drawing order: QgsLayerTree order, top first.
order = [l.layer() for l in root.findLayers()]
layers = [l for l in order if l in layers]

ms = QgsMapSettings()
ms.setLayers(layers)
ms.setDestinationCrs(QgsCoordinateReferenceSystem("EPSG:25833"))
ms.setExtent(QgsRectangle(*map(float, bbox.split(","))))
ms.setOutputSize(QSize(size, size))
ms.setOutputDpi(25.4 / 0.28)
ms.setLabelingEngineSettings(project.labelingEngineSettings())
# QGIS Server renders with UseRenderingOptimization (layer geometry simplification) on.
ms.setFlag(Qgis.MapSettingsFlag.UseRenderingOptimization, os.environ.get("NOOPT") != "1")
job = QgsMapRendererSequentialJob(ms)
job.start()
job.waitForFinished()
results = job.takeLabelingResults()
for lp in results.allLabels():
    if lp.isUnplaced:
        continue
    r = lp.labelRect
    layer = project.mapLayer(lp.layerID)
    c = lp.cornerPoints[0] if lp.cornerPoints else None
    print(json.dumps({"layer": layer.name() if layer else lp.layerID, "fid": lp.featureId, "text": lp.labelText,
                      "bbox": [r.xMinimum(), r.yMinimum(), r.xMaximum(), r.yMaximum()],
                      "origin": [c.x(), c.y()] if c else None, "rotation": lp.rotation}))
app.exitQgis()
