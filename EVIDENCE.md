# How QGIS Server works — with evidence

Everything below was checked against the QGIS source checkout in
`~/Projekte/kartverket/QGIS` (commit `b24513c`, 2026-09-27, QGIS 4.3.0-Master) and
by running the `qgis_mapserver` binary built from it (`build-release/output/Contents/MacOS`).
Paths are relative to `QGIS/src/server/`; line numbers are from that commit.

## 1. One process, one long-lived `QgsServer`, a request loop

```
qgis_map_serv.cpp:79    QgsServer server;
qgis_map_serv.cpp:91    while ( fcgi_accept() >= 0 )        // FastCGI loop
qgis_map_serv.cpp:97      server.handleRequest( request, response );
```

`qgis_mapserv.fcgi` is a FastCGI program behind Apache/nginx; `qgis_mapserver` is the
built-in development HTTP server. Both create **one** `QgsServer` (and one `QgsApplication`)
and feed every request to `QgsServer::handleRequest`. All state — loaded projects,
service modules, settings — lives across requests.

## 2. Services are plug-in modules (shared libraries)

```
qgsserver.cpp:377            const QString modulePath = QgsApplication::libexecPath() + "server";
qgsserver.cpp:379            sServiceRegistry->init( modulePath, sServerInterface );
qgsservicenativeloader.cpp:94   QLibrary lib( location );
qgsservicenativeloader.cpp:103  lib.resolve( "QGS_ServiceModule_Init" )
services/wms/qgswms.cpp:170     QGISEXTERN QgsServiceModule *QGS_ServiceModule_Init()
```

WMS, WFS, WCS, WMTS, WFS3 are separate libraries in `src/server/services/*` that register
`(name, version)` pairs. Runtime proof (log of the real server started for this work):

```
"Loading native module <build>/MacOS/server/libwfs.so"
Adding service WFS 1.1.0
Adding service WFS 1.0.0
"Loading native module <build>/MacOS/server/libwms.so"
Adding service WMS 1.3.0
Adding service WMS 1.1.1
"Loading native module <build>/MacOS/server/libwmts.so"
Adding service WMTS 1.0.0
```

…and negative proof: started with a wrong `QGIS_PREFIX_PATH`, the modules were not found and
`SERVICE=WMS` was answered with *"Service unknown or unsupported"* while the log said
`Service WMS is not registered` — the core server knows nothing about WMS itself.

The registry picks a module by `SERVICE` + `VERSION`, falling back to the default version:

```
qgsserviceregistry.cpp:93   QgsService *QgsServiceRegistry::getService( name, version )
qgsserviceregistry.cpp:111  "Service %1 %2 not found, returning default"
```

## 3. The request pipeline (`QgsServer::handleRequest`)

```
qgsserver.cpp:437   requestHandler.parseInput();                  // KVP / POST body
qgsserver.cpp:150   QgsServer::configPath(...)                    // which project?
qgsserver.cpp:493   project = mConfigCache->project( configFilePath, ... );
qgsserver.cpp:521   if ( params.service().isEmpty() )             // no SERVICE → OGC API
qgsserver.cpp:523     QgsServerApi *api = sServiceRegistry->apiForRequest( request );
qgsserver.cpp:559   QgsService *service = sServiceRegistry->getService( params.service(), params.version() );
qgsserver.cpp:562   service->executeRequest( request, responseDecorator, project );
qgsserver.cpp:596   responseDecorator.finish();                   // plugin filters, flush
```

Project selection (`configPath`, `qgsserver.cpp:150-178`): the `QGIS_PROJECT_FILE` setting
wins; otherwise the `MAP` parameter; otherwise the environment / default path.

## 4. Projects are read once and cached

```
qgsconfigcache.cpp:38-51   'periodic' / 'filesystem' (default) cache strategies
qgsconfigcache.cpp:96      const QgsProject *QgsConfigCache::project( path, settings )
qgsconfigcache.cpp:130     if ( prj->read( path, readFlags ) )
qgsconfigcache.cpp:139     wmsRestrictedLayers(...)   // bad layers only fail if published
qgsconfigcache.cpp:305     QFileSystemWatcher::fileChanged → removeChangedEntry
```

Runtime: `No cache strategy was specified. Using 'filesystem' as default.` /
`Initializing 'filesystem' cache strategy`.

`QgsProject::read` handles both formats: a `.qgz` is a zip that is unpacked first
(`core/project/qgsproject.cpp:4086` / `QgsProject::unzip` at `:4792`), a `.qgs` is the XML
itself. The XML contains `<projectlayers><maplayer>` (data source, CRS, `<renderer-v2>`
symbology), `<layer-tree-group>` (the legend/WMS layer hierarchy) and `<properties>`, where
all OWS publishing settings live:

```
qgsserverprojectutils.cpp:46    WMSServiceTitle      :294  WMSCrsList
qgsserverprojectutils.cpp:132   WMSMaxWidth          :477  WMSRootName
qgsserverprojectutils.cpp:142   WMSUseLayerIDs       :482  WMSRestrictedLayers
qgsserverprojectutils.cpp:488   WMSExtent            :513  WFSLayers
qgsserverprojectutils.cpp:518   WFSLayersPrecision (default 6)
```

## 5. WMS: dispatch and GetMap

`services/wms/qgswms.cpp:70-133` dispatches on `REQUEST` (GetCapabilities, GetMap — with
DXF/PDF variants —, GetFeatureInfo, GetLegendGraphic, GetPrint, …). GetMap:

```
qgswmsgetmap.cpp:37      VERSION is mandatory ("Please add the value of the VERSION parameter")
qgswmsgetmap.cpp:43-52   QgsWmsRenderContext with flags (opacity, filter, selection, …)
qgswmsgetmap.cpp:57      std::unique_ptr<QImage> result( renderer.getMap() );
qgswmsrenderer.cpp:1070  QgsRenderer::getMap(): layers → QgsMapSettings → render → scale
qgswmsrenderer.cpp:1427  CRS:84 → EPSG:4326 with swapped BBOX
qgswmsrenderer.cpp:1467  WMS 1.3.0 + axis-inverted CRS (EPSG:4326) → BBOX is lat/lon
qgswmsrenderer.cpp:3733  QgsMapRendererJobProxy (parallel or sequential QgsMapRendererJob)
qgswmsrendercontext.cpp:32   OGC_PX_M = 0.00028   (1 px = 0.28 mm ≈ 90.7 DPI)
qgswmsrendercontext.cpp:54   std::reverse( mLayersToRender )   (WMS order is bottom→top)
qgswmsrendercontext.cpp:289  layerNickname: short name, else name (or id with WMSUseLayerIDs)
qgswmsrendercontext.cpp:844  mapSize(): BBOX/size aspect mismatch → square pixels…
qgswmsrenderer.cpp:4201      …then scaleImage() stretches to WIDTH×HEIGHT
qgswmsgetcapabilities.cpp:810  root layer name = WMSRootName, else project title
```

GetFeatureInfo searches a rectangle around the clicked pixel whose size depends on geometry
type (`qgswmsrenderer.cpp:3482-3513`: extent width / 400 for polygons, / 200 for lines,
/ 100 for points, or `FI_*_TOLERANCE` pixels).

The actual drawing is the same `QgsMapRendererJob` → `QgsVectorLayerRenderer` →
`QgsFeatureRenderer`/`QgsSymbol` code path as QGIS Desktop, painting onto a `QImage` with
`QPainter`. This is why a server map looks exactly like the desktop map.

## 6. Observed behaviours (black-box, against `earth.qgs`)

The requests and responses were captured locally (not part of the repository); the file names
below refer to those captures and can be regenerated with `tools/compare_with_qgis.py`.

| Observation | Evidence |
|---|---|
| Root layer `Earth` (from `WMSRootName`), group `earth`, three layers | `wms_capabilities.xml` |
| WMS 1.3.0 EPSG:4326 BBOX is `minLat,minLon,maxLat,maxLon` | `getmap_world_4326.png` |
| Unknown layer → `LayerNotDefined`, HTTP 400 | `error_layer_not_defined.xml` |
| WFS publishes **only** `WFSLayers`; `earth.qgs` has none → `TypeName … could not be found` | `wfs_getfeature.json` |
| WFS KVP `BBOX` is x,y (lon,lat) even with an EPSG:4326 suffix | `wfs_getfeature_bbox.json` (lat,lon order returns nothing) |
| GeoJSON coordinates rounded to 6 decimals; `PROPERTYNAME` without geometry → `geometry: null` | `wfs_getfeature_*.json` |
| GetFeatureInfo JSON gets a `crs` member when the map CRS isn't EPSG:4326 | tools/compare_with_qgis.py |
| No `WMSMaxWidth` configured → QGIS renders a 99999 px wide image | manual request |
| **Bug:** a field named `feature::id` produces `<qgs:feature::id>` → GML that is not well-formed XML | tools/compare_with_qgis.py |
| EPSG:3857 capabilities BoundingBox for lat −89.9 is the unclamped −44 927 326 m | `wms_capabilities.xml` |
