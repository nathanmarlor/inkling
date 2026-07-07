#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <dlfcn.h>

// inklingfb — inkling's bridge into xochitl (the reMarkable UI), via xovi.
//
// Hook-free and stable: everything is done by walking the live QtQuick visual tree
// and calling xochitl's own Qt meta-methods (all entry points dlsym'd from the
// exported Qt libraries — xochitl itself is stripped). Architecture rules, each paid
// for with a device freeze or crash (full war stories in ../README.md):
//   * NO function hooks (xovi's arm32 trampoline races on hot paths).
//   * ALL Qt access happens on the GUI thread via gui_process (posted with
//     invokeMethodImpl); the worker thread only watches trigger files. The one
//     exception is grabWindow, which is panel-safe ONLY from the worker.
//   * NEVER parent an item into the selection menu (it's a Container and hoovers
//     children into its content model; teardown then crashes xochitl).
//   * NEVER delete strokes via a programmatic selection — emit the menu's own
//     deleteSelection() signal (the trash button's path) instead.
//
// File-trigger contract (each file is deleted once handled; the daemon treats the
// deletion as the ack):
//   touch /tmp/inklingfb_clear      clear the whole page (ink + text), undoable
//   touch /tmp/inkling_grab         write the live frame to /tmp/inkling_frame
//   touch /tmp/inkling_selinfo      selection info -> /tmp/inkling_selinfo_out
//                                   ("count x y w h portrait")
//   touch /tmp/inkling_seldelete    native delete of the current selection
//   echo pen|sel > /tmp/inkling_tool          native drawing-tool switch
//   echo begin|end > /tmp/inkling_spinlayer   scratch layer for the spinner
//   dev tooling: /tmp/inkling_probe (introspection dump), /tmp/inkling_selrect
//                (programmatic selection; "view x y w h [mode]" in screen px)
// Plus: the "AI" convert button is injected beside the selection menu whenever a
// menu without one exists; tapping it XHRs the daemon (127.0.0.1:9137/convert).

typedef struct { const void *data; const char *name; } GA;           // QGenericArgument {data, name}
typedef int         (*invoke_fn)(void*, const char*, int, GA,GA,GA,GA,GA,GA,GA,GA,GA,GA,GA);
typedef const char *(*classname_fn)(const void*);
typedef void        (*findchildren_fn)(const void*, const void*, void*, int);
typedef void        (*allwindows_fn)(void*);                          // sret QWindowList
typedef void        (*childitems_fn)(void*, const void*);            // sret QList<QQuickItem*>
typedef void        (*qproperty_fn)(void*, const void*, const char*); // sret QVariant

typedef int         (*propcount_fn)(const void*);                     // QMetaObject::propertyCount()
typedef void        (*property_fn)(void*, const void*, int);          // QMetaObject::property(i) -> sret QMetaProperty
typedef const char *(*propname_fn)(const void*);                      // QMetaProperty::name()
typedef const char *(*proptype_fn)(const void*);                      // QMetaProperty::typeName()
typedef int         (*methodcount_fn)(const void*);                   // QMetaObject::methodCount()
typedef void        (*method_fn)(void*, const void*, int);            // QMetaObject::method(i) -> sret QMetaMethod
typedef void        (*methodsig_fn)(void*, const void*);              // QMetaMethod::methodSignature() -> sret QByteArray
typedef const char *(*bconstdata_fn)(const void*);                    // QByteArray::constData()
typedef void          (*grabwindow_fn)(void*, void*);                 // QQuickWindow::grabWindow() -> sret QImage
typedef const unsigned char *(*qimg_bits_fn)(const void*);            // QImage::constBits()
typedef int           (*qimg_int_fn)(const void*);                    // QImage::width/height/bytesPerLine/format
typedef long          (*qimg_size_fn)(const void*);                   // QImage::sizeInBytes()
typedef void          (*qimg_dtor_fn)(void*);                         // ~QImage()
typedef int   (*methodtype_fn)(const void*);                          // QMetaMethod::methodType() (Signal==1)
// QVariant value readers — constData()/typeName() are INLINE in Qt6 (dlsym = null),
// but the conversion methods are exported. CAUTION: QRectF/QPointF/QSizeF are
// homogeneous double aggregates, which arm32 HARD-FLOAT AAPCS returns in VFP regs
// d0..d3 — NOT via sret. Model them as C structs of doubles so GCC matches the ABI.
typedef struct { double x, y, w, h; } CRectF;
typedef struct { double x, y; }       CPointF;
typedef char   (*v_tobool_fn)(const void*);                            // QVariant::toBool()
typedef int    (*v_toint_fn)(const void*, void*);                      // QVariant::toInt(bool*)
typedef double (*v_todouble_fn)(const void*, void*);                   // QVariant::toDouble(bool*)
typedef CRectF (*v_torectf_fn)(const void*);                           // QVariant::toRectF() — HFA return
typedef CPointF(*v_topointf_fn)(const void*);                          // QVariant::toPointF() — HFA return
typedef CPointF(*v_tosizef_fn)(const void*);                           // QVariant::toSizeF() — HFA return
typedef void   (*v_dtor_fn)(void*);                                    // ~QVariant()
// --- runtime QML injection (the Convert button) ---
typedef char  (*invokeimpl_fn)(void*, void*, int, void*);              // QMetaObject::invokeMethodImpl(obj, slotObj, type, ret)
typedef void* (*qmlengine_fn)(const void*);                            // qmlEngine(QObject*)
typedef void* (*qmlcontext_fn)(const void*);                           // qmlContext(QObject*)
typedef void  (*qcomp_ctor_fn)(void*, void*, void*);                   // QQmlComponent(engine, parent)
typedef void  (*qcomp_setdata_fn)(void*, const void*, const void*);    // setData(QByteArray, QUrl)
typedef void* (*qcomp_create_fn)(void*, void*);                        // create(QQmlContext*)
typedef void  (*qcomp_dtor_fn)(void*);
typedef void  (*qba_ctor_fn)(void*, const char*, int);                 // QByteArray(const char*, qsizetype)
typedef void  (*qurl_ctor_fn)(void*);                                  // QUrl()
typedef void  (*qurl_dtor_fn)(void*);
typedef char  (*setprop_fn)(void*, const char*, const void*);          // QObject::setProperty(name, QVariant)
typedef void  (*qvar_i_fn)(void*, int);                                // QVariant(int)
typedef void  (*qvar_d_fn)(void*, double);                             // QVariant(double)
typedef void  (*setparentitem_fn)(void*, void*);                       // QQuickItem::setParentItem
typedef void  (*setparent_fn)(void*, void*);                           // QObject::setParent
typedef int         (*enumcount_fn)(const void*);                      // QMetaObject::enumeratorCount()
typedef void        (*enumerator_fn)(void*, const void*, int);         // QMetaObject::enumerator(i) -> sret QMetaEnum
typedef const char *(*enumname_fn)(const void*);                       // QMetaEnum::name()
typedef int         (*enumkeycount_fn)(const void*);                   // QMetaEnum::keyCount()
typedef const char *(*enumkey_fn)(const void*, int);                   // QMetaEnum::key(i)
typedef int         (*enumvalue_fn)(const void*, int);                 // QMetaEnum::value(i)

static invoke_fn       p_invoke;
static classname_fn    p_className;
static findchildren_fn p_findchildren;
static allwindows_fn   p_allwindows;
static childitems_fn   p_childitems;
static qproperty_fn    p_qproperty;
static propcount_fn    p_propcount;
static property_fn     p_property;
static propname_fn     p_propname;
static proptype_fn     p_proptype;
static methodcount_fn  p_methodcount;
static method_fn       p_method;
static methodsig_fn    p_methodsig;
static bconstdata_fn   p_bconstdata;
static grabwindow_fn   p_grabwindow;
static qimg_bits_fn    p_qimg_bits;
static qimg_int_fn     p_qimg_w, p_qimg_h, p_qimg_bpl, p_qimg_fmt;
static qimg_size_fn    p_qimg_size;
static qimg_dtor_fn    p_qimg_dtor;
static methodtype_fn   p_methodtype;
static v_tobool_fn     p_v_tobool;
static v_toint_fn      p_v_toint;
static v_todouble_fn   p_v_todouble;
static v_torectf_fn    p_v_torectf;
static v_topointf_fn   p_v_topointf;
static v_tosizef_fn    p_v_tosizef;
static v_dtor_fn       p_v_dtor;
static invokeimpl_fn   p_invokeimpl;
static void          **p_qapp_self;    // &QCoreApplication::self
static qmlengine_fn    p_qmlengine;
static qmlcontext_fn   p_qmlcontext;
static qcomp_ctor_fn   p_qcomp_ctor;
static qcomp_setdata_fn p_qcomp_setdata;
static qcomp_create_fn p_qcomp_create;
static qcomp_dtor_fn   p_qcomp_dtor;
static qba_ctor_fn     p_qba_ctor;
static qurl_ctor_fn    p_qurl_ctor;
static qurl_dtor_fn    p_qurl_dtor;
static setprop_fn      p_setprop;
static qvar_i_fn       p_qvar_i;
static qvar_d_fn       p_qvar_d;
static setparentitem_fn p_setparentitem;
static setparent_fn    p_setparent;
static enumcount_fn    p_enumcount;
static enumerator_fn   p_enumerator;
static enumname_fn     p_enumname;
static enumkeycount_fn p_enumkeycount;
static enumkey_fn      p_enumkey;
static enumvalue_fn    p_enumvalue;
static void           *g_qobj_smo;    // &QObject::staticMetaObject


// --- tiny Qt introspection helpers (all read-only, safe on any QObject) ---
static void* meta_of(void *o){ return ((void*(*)(void*))(*(void***)o)[0])(o); }   // metaObject() @ vtable[0]
static const char* cls(void *o){ if(!o) return 0; void *mo=meta_of(o); return mo?p_className(mo):0; }
static void* read_obj_prop(void *o, const char *name){
    char v[64]; for(int i=0;i<64;i++) v[i]=0; p_qproperty(v,o,name); return *(void**)v; // ptr stored inline in QVariant
}
// QMetaObject::superClass() is inline; superdata is the first field of QMetaObject.
static int is_quickitem(void *o){
    void *mo=meta_of(o);
    for(int i=0; mo && i<40; i++){ const char*c=p_className(mo); if(c&&!strcmp(c,"QQuickItem")) return 1; mo=*(void**)mo; }
    return 0;
}
static int is_class(void *o, const char *want){
    void *mo=meta_of(o);
    for(int i=0; mo && i<40; i++){ const char*c=p_className(mo); if(c&&!strcmp(c,want)) return 1; mo=*(void**)mo; }
    return 0;
}

// --- collect the active page's SceneController(s) + scene views ---
// The g_* scratch arrays below are touched ONLY from the GUI thread (gui_process) —
// no lock needed. The worker thread never calls into Qt or these arrays.
static void *g_seen[9000]; static int g_nseen;
static void *g_scs[16];    static int g_nscs;
static void *g_views[16];  static int g_nviews;
static void *g_selitems[32]; static int g_nselitems;   // visual items with "Select" in class
static int seen(void *p){ for(int i=0;i<g_nseen;i++) if(g_seen[i]==p) return 1; if(g_nseen<9000) g_seen[g_nseen++]=p; return 0; }

static void add_sc(void *sc){
    if(!sc){ return; } const char *c=cls(sc); if(!c||strcmp(c,"SceneController")) return;
    for(int i=0;i<g_nscs;i++) if(g_scs[i]==sc) return;
    if(g_nscs<16) g_scs[g_nscs++]=sc;
}
static void add_view(void *v){ for(int i=0;i<g_nviews;i++) if(g_views[i]==v) return; if(g_nviews<16) g_views[g_nviews++]=v; }

// Visual-only walk (QQuickItem::childItems) — the stable traversal. Reaches the
// DocumentView / DeviceSceneView that are actually rendered (i.e. the active page).
static void walk(void *item, int depth){
    if(!item || depth>70 || g_nseen>8000 || seen(item)) return;
    const char *c = cls(item); if(!c) return;
    if(!strcmp(c,"SceneController")){ add_sc(item); return; }
    if((strstr(c,"Select")||strstr(c,"select")) && g_nselitems<32) g_selitems[g_nselitems++]=item;
    if(strstr(c,"DocumentView") && !strstr(c,"Shortcuts")){ add_sc(read_obj_prop(item,"sceneController")); add_view(item); }
    else if(strstr(c,"DeviceScene")){ add_sc(read_obj_prop(item,"controller")); add_view(item); }
    if(is_quickitem(item)){
        void *cl[3]={0,0,0}; p_childitems(&cl,item);
        void **kids=(void**)cl[1]; long kn=(long)cl[2];
        for(long i=0;i<kn;i++) walk(kids[i], depth+1);
    }
}

static void locate(void){
    g_nseen=0; g_nscs=0; g_nviews=0; g_nselitems=0;
    void *wl[3]={0,0,0}; p_allwindows(&wl);
    void **wins=(void**)wl[1]; long wn=(long)wl[2];
    for(long w=0; w<wn; w++){
        void *lst[3]={0,0,0}; p_findchildren(wins[w], g_qobj_smo, lst, 1);   // find QQuickRootItem
        void **arr=(void**)lst[1]; long n=(long)lst[2];
        for(long j=0;j<n;j++){ const char *c=cls(arr[j]); if(c && !strcmp(c,"QQuickRootItem")) walk(arr[j],0); }
    }
}

static void clear_page(void){
    locate();
    GA z = {0,0};
    for(int i=0;i<g_nscs;i++){
        p_invoke(g_scs[i], "clearLines",        2, z,z,z,z,z,z,z,z,z,z,z);   // ink
        p_invoke(g_scs[i], "clearRootDocument", 2, z,z,z,z,z,z,z,z,z,z,z);   // text
    }
    for(int i=0;i<g_nviews;i++)
        p_invoke(g_views[i], "update", 2, z,z,z,z,z,z,z,z,z,z,z);            // refresh the panel
    fprintf(stderr, "[inklingfb] cleared page (%d controller(s), %d view(s))\n", g_nscs, g_nviews);
}

// PROBE (temporary, read-only): full introspection dump — every property (with live
// typed values) and every method — of the SceneControllers, DocumentViews, and any
// selection-owning QObject reachable from their object-pointer properties (e.g.
// DocumentView.pageSelection). Goal: find the NATIVE selection bounds + delete.
static void dump_qobject(FILE *fp, void *o, const char *label){
    if(!o){ fprintf(fp, "\n== %s : (null) ==\n", label); return; }
    void *mo = meta_of(o);
    const char *cn = mo ? p_className(mo) : 0;
    int n = (p_propcount && mo) ? p_propcount(mo) : -1;
    fprintf(fp, "\n== %s : class %s : %d properties ==\n", label, cn?cn:"?", n);
    for(int p=0; p<n; p++){
        char mp[32]; for(int k=0;k<32;k++) mp[k]=0; p_property(mp, mo, p);   // QMetaProperty (POD)
        const char *nm = p_propname ? p_propname(mp) : 0;
        const char *tn = p_proptype ? p_proptype(mp) : 0;
        fprintf(fp, "  P %-30s %-24s", nm?nm:"?", tn?tn:"?");
        // WHITELIST which property types we READ: invoking arbitrary QML getters from
        // this worker thread crashes xochitl (QQuickAnchorLine did). Only simple value
        // types and QObject pointers are safe to fetch.
        int is_ptr   = tn && strchr(tn,'*') && !strstr(tn,"QQmlListProperty");
        int is_enum  = tn && strstr(tn,"::") && !strchr(tn,'*');
        int readable = tn && (is_ptr || is_enum ||
            !strcmp(tn,"bool") || !strcmp(tn,"int") || !strcmp(tn,"uint") ||
            !strcmp(tn,"double") || !strcmp(tn,"qreal") || !strcmp(tn,"float") ||
            strstr(tn,"QFlags") || !strcmp(tn,"QRectF") || !strcmp(tn,"QPointF") || !strcmp(tn,"QSizeF"));
        if(nm && readable){
            char v[64]; for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, o, nm);
            if(!strcmp(tn,"bool") && p_v_tobool)                  fprintf(fp, " = %d", (int)p_v_tobool(v));
            else if((!strcmp(tn,"int")||!strcmp(tn,"uint")||is_enum||strstr(tn,"QFlags")) && p_v_toint)
                                                                  fprintf(fp, " = %d", p_v_toint(v,0));
            else if((!strcmp(tn,"double")||!strcmp(tn,"qreal")||!strcmp(tn,"float")) && p_v_todouble)
                                                                  fprintf(fp, " = %.2f", p_v_todouble(v,0));
            else if(!strcmp(tn,"QRectF") && p_v_torectf){ CRectF r = p_v_torectf(v); fprintf(fp, " = [%.1f %.1f %.1f %.1f]", r.x,r.y,r.w,r.h); }
            else if(!strcmp(tn,"QPointF") && p_v_topointf){ CPointF r = p_v_topointf(v); fprintf(fp, " = [%.1f %.1f]", r.x,r.y); }
            else if(!strcmp(tn,"QSizeF") && p_v_tosizef){ CPointF r = p_v_tosizef(v); fprintf(fp, " = [%.1f %.1f]", r.x,r.y); }
            else if(is_ptr){ void *po=*(void**)v; const char *pc=po?cls(po):0; fprintf(fp, " = %p (%s)", po, pc?pc:(po?"?":"null")); }
            if(p_v_dtor) p_v_dtor(v);
        }
        fprintf(fp, "\n");
    }
    int mn = (p_methodcount && mo) ? p_methodcount(mo) : -1;
    fprintf(fp, "  -- methods: %d --\n", mn);
    for(int m=0;m<mn;m++){
        char mm[32]; for(int k=0;k<32;k++) mm[k]=0; p_method(mm, mo, m);
        char qb[16]; for(int k=0;k<16;k++) qb[k]=0; p_methodsig(qb, mm);
        // QByteArray::constData() is inline (not a symbol); Qt6 QByteArray is
        // {Data* d; char* ptr; qsizetype size} so the C string is at offset 4.
        const char *s = *(const char**)(qb+4);
        int mt = p_methodtype ? p_methodtype(mm) : -1;               // 0=Method 1=Signal 2=Slot
        if(s && *s) fprintf(fp, "    [%c] %s\n", mt==1?'S':(mt==2?'L':'M'), s);
    }
    int en = (p_enumcount && mo) ? p_enumcount(mo) : 0;
    if(en > 0) fprintf(fp, "  -- enums: %d --\n", en);
    for(int e=0;e<en;e++){
        char me[32]; for(int k=0;k<32;k++) me[k]=0; p_enumerator(me, mo, e);   // QMetaEnum (POD)
        fprintf(fp, "    %s {", p_enumname?p_enumname(me):"?");
        int kc = p_enumkeycount ? p_enumkeycount(me) : 0;
        for(int k=0;k<kc;k++) fprintf(fp, " %s=%d", p_enumkey(me,k), p_enumvalue(me,k));
        fprintf(fp, " }\n");
    }
}
// Dump any QObject* property of `o` whose live class name mentions selection.
static void dump_selectionish_children(FILE *fp, void *o, const char *label){
    void *mo = meta_of(o); if(!mo) return;
    int n = p_propcount ? p_propcount(mo) : 0;
    for(int p=0; p<n; p++){
        char mp[32]; for(int k=0;k<32;k++) mp[k]=0; p_property(mp, mo, p);
        const char *nm = p_propname ? p_propname(mp) : 0;
        const char *tn = p_proptype ? p_proptype(mp) : 0;
        if(!nm || !tn || !strchr(tn,'*')) continue;
        void *po = read_obj_prop(o, nm); if(!po) continue;
        const char *pc = cls(po); if(!pc) continue;
        if(strstr(pc,"Select") || strstr(pc,"select") || strstr(nm,"Selection") || strstr(nm,"selection")){
            char lbl[128]; snprintf(lbl, sizeof lbl, "%s.%s", label, nm);
            dump_qobject(fp, po, lbl);
        }
    }
}
static void probe_selection(void){
    locate();
    FILE *fp = fopen("/tmp/inkling_probe_out","w");
    if(!fp) return;
    fprintf(fp, "controllers=%d views=%d\n", g_nscs, g_nviews);
    for(int i=0;i<g_nscs;i++){
        char lbl[64]; snprintf(lbl, sizeof lbl, "SceneController[%d]", i);
        dump_qobject(fp, g_scs[i], lbl);
        dump_selectionish_children(fp, g_scs[i], lbl);
    }
    for(int i=0;i<g_nviews;i++){
        const char *vc = cls(g_views[i]);
        if(!vc || !strstr(vc,"DocumentView")) continue;   // full dump of the DocumentView only
        char lbl[64]; snprintf(lbl, sizeof lbl, "DocumentView[%d]", i);
        dump_qobject(fp, g_views[i], lbl);
        dump_selectionish_children(fp, g_views[i], lbl);
        // The selection-owning objects, dumped even when the generic scan misses them.
        dump_qobject(fp, read_obj_prop(g_views[i], "pageSelection"),     "DocumentView.pageSelection");
        dump_qobject(fp, read_obj_prop(g_views[i], "movePageSelection"), "DocumentView.movePageSelection");
        // Tool-switching candidates (for the native pen/selection tool switch).
        dump_qobject(fp, read_obj_prop(g_views[i], "penInput"),             "DocumentView.penInput");
        dump_qobject(fp, read_obj_prop(g_views[i], "toolbarConfiguration"), "DocumentView.toolbarConfiguration");
        dump_qobject(fp, read_obj_prop(g_views[i], "toolbarProvider"),      "DocumentView.toolbarProvider");
        dump_qobject(fp, read_obj_prop(g_views[i], "penHandler"),           "DocumentView.penHandler");
    }
    for(int i=0;i<g_nselitems;i++){
        char lbl[64]; snprintf(lbl, sizeof lbl, "SelectItem[%d]", i);
        dump_qobject(fp, g_selitems[i], lbl);
    }
    fclose(fp);
    fprintf(stderr, "[inklingfb] probe done (%d controllers, %d views)\n", g_nscs, g_nviews);
}


// Live VIEW→SCENE offset, learned whenever a selection exists (selinfo reads both
// sceneSelectionRect and viewSelectionRect; the difference is the viewport pan).
// NEVER hardcode this: the scene origin moves with page scroll/zoom — a stale
// offset once selected (and nearly deleted) a completely different page region.
static double g_scene_dx = -702.0, g_scene_dy = 0.0;

// NATIVE rect selection: SceneController::addSelectionRect(QRect, LineSelectionMode).
// Trigger file /tmp/inkling_selrect contains "x y w h [mode]" in SCENE coords, or
// "view x y w h [mode]" in VIEW (screen) px — converted with the cached offset.
// NB QRect stores x1,y1,x2,y2 (not w,h).
static void select_rect(void){
    int x=0,y=0,w=0,h=0,mode=0,is_view=0;
    char head[8]={0};
    FILE *tf = fopen("/tmp/inkling_selrect","r");
    if(tf){
        if(fscanf(tf, "%7s", head)==1 && !strcmp(head,"view")){
            is_view = 1;
            if(fscanf(tf, "%d %d %d %d %d", &x,&y,&w,&h,&mode) < 4){ w=h=0; }
        } else {
            x = atoi(head);
            if(fscanf(tf, "%d %d %d %d", &y,&w,&h,&mode) < 3){ w=h=0; }
        }
        fclose(tf);
    }
    if(is_view){ x += (int)g_scene_dx; y += (int)g_scene_dy; }
    if(w<=0 || h<=0){ fprintf(stderr, "[inklingfb] selrect: bad args\n"); return; }
    locate();
    GA z = {0,0};
    int rect[4] = { x, y, x+w-1, y+h-1 };
    GA ar = {rect, "QRect"};
    GA am = {&mode, "SceneController::LineSelectionMode"};
    for(int i=0;i<g_nscs;i++){
        int rd = p_invoke(g_scs[i], "addSelectionRect", 2, z, ar, am, z,z,z,z,z,z,z,z);
        fprintf(stderr, "[inklingfb] addSelectionRect(%d,%d,%d,%d mode=%d) sc%d matched=%d\n", x,y,w,h,mode,i,rd);
    }
    for(int i=0;i<g_nviews;i++)
        p_invoke(g_views[i], "update", 2, z,z,z,z,z,z,z,z,z,z,z);
}

// Native selection info for the daemon: viewSelectionRect (VIEW px — tight bbox of the
// selected strokes) + selectionItemCount. Written to /tmp/inkling_selinfo_out as
// "count x y w h". Trigger: touch /tmp/inkling_selinfo
static void selection_info(void){
    locate();
    FILE *fp = fopen("/tmp/inkling_selinfo_out.tmp","w");
    if(!fp) return;
    int count = 0; CRectF vr = {0,0,0,0}; void *active_sc = 0;
    for(int i=0;i<g_nselitems && p_v_toint && p_v_torectf;i++){
        const char *c = cls(g_selitems[i]);
        if(!c || !strstr(c,"SceneSelectionHandler")) continue;
        char v[64]; for(int k=0;k<64;k++) v[k]=0;
        p_qproperty(v, g_selitems[i], "viewSelectionRect");
        vr = p_v_torectf(v); if(p_v_dtor) p_v_dtor(v);
        { // refresh the live view→scene offset while both rects exist
            char v3[64]; for(int k=0;k<64;k++) v3[k]=0;
            p_qproperty(v3, g_selitems[i], "sceneSelectionRect");
            CRectF sr = p_v_torectf(v3); if(p_v_dtor) p_v_dtor(v3);
            if(vr.w > 0.0 && sr.w > 0.0){ g_scene_dx = sr.x - vr.x; g_scene_dy = sr.y - vr.y; }
        }
        active_sc = read_obj_prop(g_selitems[i], "controller");
        if(active_sc){ char v2[64]; for(int k=0;k<64;k++) v2[k]=0; p_qproperty(v2, active_sc, "selectionItemCount"); count = p_v_toint(v2,0); if(p_v_dtor) p_v_dtor(v2); }
        break;
    }
    // DocumentView.portrait — how the tablet is held, so the daemon uprights the
    // sketch for the model. xochitl POOLS DocumentViews, so read it from the one that
    // owns the ACTIVE selection's controller (matching the first DocumentView blindly
    // returned a stale pooled value → the orientation flip-flopped run to run).
    int portrait = 1;   // default portrait (the native reMarkable orientation)
    void *doc_view = 0;
    for(int i=0;i<g_nviews;i++){
        const char *vc = cls(g_views[i]);
        if(!vc || !strstr(vc,"DocumentView")) continue;
        if(active_sc && read_obj_prop(g_views[i],"sceneController") == active_sc){ doc_view = g_views[i]; break; }
        if(!doc_view) doc_view = g_views[i];   // fallback to first if no match
    }
    if(doc_view){
        char v[64]; for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, doc_view, "portrait");
        portrait = p_v_tobool ? (int)p_v_tobool(v) : 1; if(p_v_dtor) p_v_dtor(v);
    }
    fprintf(fp, "%d %.1f %.1f %.1f %.1f %d\n", count, vr.x, vr.y, vr.w, vr.h, portrait);
    fclose(fp);
    rename("/tmp/inkling_selinfo_out.tmp","/tmp/inkling_selinfo_out");
    fprintf(stderr, "[inklingfb] selinfo count=%d rect=[%.1f %.1f %.1f %.1f] portrait=%d\n", count, vr.x, vr.y, vr.w, vr.h, portrait);
}

// Spinner scratch layer: the loading spinner is drawn on its OWN temporary layer so
// removing it is a plain deleteLayer — NOT a programmatic selection + deleteSelection,
// which crashed xochitl twice ("We crashed", exit 1 → device reboot). Trigger file
// /tmp/inkling_spinlayer contains "begin" (add layer + make it current) or "end"
// (restore the previous layer and delete the scratch one). All ops are queued slot
// invokes on the SceneController, executed in order on the GUI thread.
static int g_spin_layer = -1, g_prev_layer = 0;
static int read_int_prop(void *o, const char *name, int dflt){
    if(!o || !p_v_toint) return dflt;
    char v[64]; for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, o, name);
    int r = p_v_toint(v, 0); if(p_v_dtor) p_v_dtor(v);
    return r;
}
static void spin_layer(void){
    char cmd[8]={0};
    FILE *tf = fopen("/tmp/inkling_spinlayer","r");
    if(tf){ if(fscanf(tf, "%7s", cmd)!=1) cmd[0]=0; fclose(tf); }
    locate();
    if(!g_nscs){ fprintf(stderr, "[inklingfb] spinlayer: no controller\n"); g_spin_layer=-1; return; }
    void *sc = g_scs[0];
    GA z = {0,0};
    // QUEUED invokes (type 2): a Direct invoke of setCurrentLayer left currentLayer
    // unchanged on readback (its effect is internally deferred). The daemon just waits
    // a fixed beat after this trigger is consumed — a layerCount-readback confirm was
    // tried and removed (the count readback is also deferred so it never confirmed,
    // and its rapid re-polling wedged the panel).
    if(!strcmp(cmd,"begin")){
        g_prev_layer = read_int_prop(sc, "currentLayer", 0);
        int n = read_int_prop(sc, "layerCount", 1);
        p_invoke(sc, "addLayer", 2, z,z,z,z,z,z,z,z,z,z,z);           // new layer index = n
        GA ai = {&n, "int"};
        p_invoke(sc, "setCurrentLayer", 2, z, ai, z,z,z,z,z,z,z,z,z);
        g_spin_layer = n;
        fprintf(stderr, "[inklingfb] spinlayer begin (scratch=%d, prev=%d)\n", n, g_prev_layer);
    } else if(!strcmp(cmd,"end") && g_spin_layer >= 0){
        GA ap = {&g_prev_layer, "int"};
        GA as = {&g_spin_layer, "int"};
        p_invoke(sc, "setCurrentLayer", 2, z, ap, z,z,z,z,z,z,z,z,z);
        p_invoke(sc, "deleteLayer",     2, z, as, z,z,z,z,z,z,z,z,z);
        fprintf(stderr, "[inklingfb] spinlayer end (deleted %d, restored %d)\n", g_spin_layer, g_prev_layer);
        g_spin_layer = -1;
    }
    for(int i=0;i<g_nviews;i++)
        p_invoke(g_views[i], "update", 2, z,z,z,z,z,z,z,z,z,z,z);
}

// THE clean native delete: emit the SceneSelectionHandler's deleteSelection() signal —
// exactly what the selection menu's trash button does. The QML handler runs xochitl's
// own delete with the correct internal edit id. Trigger: touch /tmp/inkling_seldelete
static void selection_delete_native(void){
    locate();
    GA z = {0,0};
    int hit = 0;
    for(int i=0;i<g_nselitems;i++){
        const char *c = cls(g_selitems[i]);
        if(!c || !strstr(c,"SceneSelectionHandler")) continue;
        int rd = p_invoke(g_selitems[i], "deleteSelection", 2, z,z,z,z,z,z,z,z,z,z,z);
        fprintf(stderr, "[inklingfb] emit deleteSelection on %s matched=%d\n", c, rd);
        hit++;
    }
    if(!hit) fprintf(stderr, "[inklingfb] seldelete: no SceneSelectionHandler found\n");
}

// --- GUI-thread executor -----------------------------------------------------------
// Runs a plain C function on xochitl's GUI thread via the exported 4-arg
// QMetaObject::invokeMethodImpl(QObject*, QSlotObjectBase*, ConnectionType, void*).
// The handmade slot object stores the impl fn in BOTH of its first two words, so it is
// correct under either QSlotObjectBase field order ({ref,impl} or {impl,ref}); the
// other word being a function pointer just acts as a refcount that never reaches 0.
typedef void (*gui_job_fn)(void);
static volatile gui_job_fn g_gui_job;
static void gui_exec_impl(void *a1, void *a2, void **a3, int a4, char *a5){
    (void)a2;(void)a3;(void)a5;
    // Handle both ImplFn conventions: Qt >=6.5 (this, recv, args, WHICH, ret) and the
    // older (WHICH, this, recv, args, ret). a1 as a real this-pointer is a big address.
    int which = ((unsigned long)a1 < 8ul) ? (int)(unsigned long)a1 : a4;
    if(which==1){ gui_job_fn f = g_gui_job; if(f){ g_gui_job = 0; f(); } }
}
static void *g_exec_slot[4] = { (void*)gui_exec_impl, (void*)gui_exec_impl, 0, 0 };
static int run_on_gui(gui_job_fn f){
    void *app = p_qapp_self ? *p_qapp_self : 0;
    if(!p_invokeimpl || !app) return 0;
    g_gui_job = f;
    p_invokeimpl(app, g_exec_slot, 2 /*QueuedConnection*/, 0);
    for(int i=0;i<50 && g_gui_job;i++) usleep(20000);   // wait <=1s for the job to run
    return g_gui_job == 0;
}

// --- the Convert button --------------------------------------------------------------
// A plain VISUAL CHILD of xochitl's SelectionContextualMenu item — deliberately NOT
// inserted into the menu's Container (insertItem): the Container's layout/teardown
// code runs over its content items on every deleteSelection, and a foreign item in
// there crashed xochitl intermittently ("We crashed" right after our seldelete emit).
// As a visual child it rides the menu's position/visibility for free, is destroyed
// with the menu, and no xochitl code path ever iterates over it. The tap fires an XHR
// to the inkling daemon's local HTTP endpoint. Re-injected by the periodic GUI job
// whenever the menu exists without it (marker property `inklingBtn`).
static const char *CONVERT_QML =
    "import QtQuick\n"
    "Rectangle {\n"
    "  property bool inklingBtn: true\n"
    "  width: 84; height: 84\n"
    "  color: \"white\"\n"
    "  border.color: \"black\"; border.width: 2\n"
    // Plain text, not a symbol glyph — the device fonts lack ★ (renders as tofu).
    "  Text { anchors.centerIn: parent; text: \"AI\"; font.pixelSize: 32; font.bold: true; color: \"black\" }\n"
    // TapHandler, not MouseArea: xochitl's stock menu buttons use TapHandler and the
    // touchscreen delivers raw touch events — MouseArea (mouse-synthesis) never fired.
    // ReleaseWithinBounds grabs at press so the tap doesn't fall through to the page.
    "  TapHandler {\n"
    "    gesturePolicy: TapHandler.ReleaseWithinBounds\n"
    "    onTapped: { var x = new XMLHttpRequest();\n"
    "      x.open(\"GET\", \"http://127.0.0.1:9137/convert\"); x.send(); }\n"
    "  }\n"
    "}\n";

static void set_prop_double(void *o, const char *n, double v);   // defined with the tool switch below
static void set_prop_int(void *o, const char *n, int v);
static void gui_add_convert_button(void){   // GUI THREAD ONLY (called from gui_process)
    locate();
    void *menu = 0;
    for(int i=0;i<g_nselitems;i++){
        const char *c = cls(g_selitems[i]);
        if(c && strstr(c,"SelectionContextualMenu")){ menu = g_selitems[i]; break; }
    }
    if(!menu) return;                                     // no menu yet (no doc open / not created)
    // NEVER give the menu itself a child: it is a Qt Quick Container, and a Container
    // HOOVERS any added visual child into its content model (that's how the button
    // kept landing inside the ButtonRow — appended to the row, stretching the menu —
    // even though insertItem was long gone). Parent to the menu's parent instead:
    // the SceneSelectionHandler, a plain Item that leaves its children alone.
    void *handler = read_obj_prop(menu, "parent");
    if(!handler) return;
    double mx=0, my=0, mw=324, mvis=1;
    { char v[64];
      for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, menu, "x");
      if(p_v_todouble) mx = p_v_todouble(v,0); if(p_v_dtor) p_v_dtor(v);
      for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, menu, "y");
      if(p_v_todouble) my = p_v_todouble(v,0); if(p_v_dtor) p_v_dtor(v);
      for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, menu, "width");
      if(p_v_todouble){ double w = p_v_todouble(v,0); if(w > 1.0) mw = w; } if(p_v_dtor) p_v_dtor(v);
      for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, menu, "visible");
      if(p_v_tobool) mvis = (double)p_v_tobool(v); if(p_v_dtor) p_v_dtor(v); }
    { // existing button on the handler? then just keep it tracking the menu box
        void *cl[3]={0,0,0}; p_childitems(&cl, handler);
        void **kids=(void**)cl[1]; long kn=(long)cl[2];
        for(long i=0;i<kn;i++){
            char v[64]; for(int k=0;k<64;k++) v[k]=0; p_qproperty(v, kids[i], "inklingMark");
            int has = p_v_toint ? p_v_toint(v,0) : 0; if(p_v_dtor) p_v_dtor(v);
            if(has == 1){
                set_prop_double(kids[i], "x", mx + mw + 12.0);
                set_prop_double(kids[i], "y", my);
                set_prop_int(kids[i], "visible", mvis > 0.5 ? 1 : 0);
                return;
            }
        }
    }
    void *engine = p_qmlengine ? p_qmlengine(menu) : 0;
    if(!engine){ fprintf(stderr,"[inklingfb] convert-btn: no qml engine\n"); return; }
    // The component is built ONCE and kept alive for the process lifetime —
    // destroying it after create() breaks property resolution on its objects.
    static char comp[32]; static void *comp_engine = 0;
    if(comp_engine != engine){
        for(int k=0;k<32;k++) comp[k]=0;
        p_qcomp_ctor(comp, engine, 0);
        static char ba[16]; static int ba_init = 0;       // built once (dtor is inline)
        if(!ba_init){ p_qba_ctor(ba, CONVERT_QML, (int)strlen(CONVERT_QML)); ba_init = 1; }
        char url[32]; for(int k=0;k<32;k++) url[k]=0; p_qurl_ctor(url);
        p_qcomp_setdata(comp, ba, url);
        p_qurl_dtor(url);
        comp_engine = engine;
    }
    void *item = p_qcomp_create(comp, p_qmlcontext(menu));
    if(item && p_setparentitem && p_setparent){
        set_prop_int(item, "inklingMark", 1);   // dynamic property — the dedup marker
        set_prop_double(item, "x", mx + mw + 12.0);
        set_prop_double(item, "y", my);
        p_setparent(item, handler);        // QObject ownership: dies with the handler
        p_setparentitem(item, handler);    // visual sibling of the menu, not content
        { // recheck: our item must be a visual child of the handler
            void *cl[3]={0,0,0}; p_childitems(&cl, handler);
            void **kids=(void**)cl[1]; long kn=(long)cl[2];
            int found = 0; for(long i=0;i<kn;i++) if(kids[i]==item) found=1;
            fprintf(stderr, "[inklingfb] convert button attached item=%p handler=%p at (%.0f,%.0f) inList=%d\n",
                    item, handler, mx + mw + 12.0, my, found);
        }
    } else fprintf(stderr, "[inklingfb] convert-btn: component create failed\n");
}

// --- native tool switch ---------------------------------------------------------------
// The active drawing tool lives on DocumentView.penHandler (ScenePenInputHandler):
//   pen (ballpoint):  lineTool=15  gestureMode=1 (WritingToolGestures)  thickness=1.0
//   selection tool:   lineTool=11  gestureMode=6 (SelectionGestures)    thickness=2.0
// Property writes must happen on the GUI thread → GUI executor job. The toolbar icon is
// not touched; the daemon switches to pen only while it injects, then restores.
// Trigger: echo pen|sel > /tmp/inkling_tool
static volatile int g_tool_req;   // 1 = pen, 2 = selection
static void set_prop_int(void *o, const char *n, int v){
    char var[64]; for(int k=0;k<64;k++) var[k]=0; p_qvar_i(var, v);
    p_setprop(o, n, var); if(p_v_dtor) p_v_dtor(var);
}
static void set_prop_double(void *o, const char *n, double v){
    char var[64]; for(int k=0;k<64;k++) var[k]=0; p_qvar_d(var, v);
    p_setprop(o, n, var); if(p_v_dtor) p_v_dtor(var);
}
static void gui_set_tool(void){   // GUI THREAD ONLY (called from gui_process)
    locate();
    void *ph = 0;
    for(int i=0;i<g_nviews && !ph;i++){
        const char *vc = cls(g_views[i]);
        if(vc && strstr(vc,"DocumentView")) ph = read_obj_prop(g_views[i], "penHandler");
    }
    if(ph && p_setprop && p_qvar_i && p_qvar_d){
        int pen = (g_tool_req == 1);
        set_prop_int   (ph, "lineTool",      pen ? 15 : 11);
        set_prop_int   (ph, "gestureMode",   pen ? 1  : 6);
        set_prop_double(ph, "lineThickness", pen ? 1.0 : 2.0);
        fprintf(stderr, "[inklingfb] tool -> %s (native penHandler write)\n", pen ? "pen" : "selection");
    } else fprintf(stderr, "[inklingfb] tool switch: no penHandler\n");
}

// Capture the live screen via QQuickWindow::grabWindow() — renders the scene to a
// QImage on demand (no /proc/pid/mem, no address drift). Writes a 20-byte header
// (w, h, bytesPerLine, format, nbytes as int32) + raw pixels to /tmp/inkling_frame.
static void grab_screen(void){
    if(!p_grabwindow || !p_qimg_bits){ fprintf(stderr,"[inklingfb] grab: missing symbols\n"); return; }
    void *wl[3]={0,0,0}; p_allwindows(&wl);
    void **wins=(void**)wl[1]; long wn=(long)wl[2];
    void *win=0;
    for(long w=0; w<wn; w++){ if(is_class(wins[w],"QQuickWindow")){ win=wins[w]; break; } }
    if(!win){ fprintf(stderr,"[inklingfb] grab: no QQuickWindow\n"); return; }
    char qi[64]; for(int k=0;k<64;k++) qi[k]=0;
    p_grabwindow(qi, win);                         // sret QImage (a fresh snapshot copy)
    const unsigned char *bits = p_qimg_bits(qi);
    int w=p_qimg_w(qi), h=p_qimg_h(qi), bpl=p_qimg_bpl(qi), fmt=p_qimg_fmt(qi);
    long nb=p_qimg_size(qi);
    if(bits && w>0 && h>0 && nb>0){
        FILE *fp=fopen("/tmp/inkling_frame.tmp","wb");
        if(fp){
            int hdr[5]={w,h,bpl,fmt,(int)nb};
            fwrite(hdr,4,5,fp); fwrite(bits,1,(size_t)nb,fp); fclose(fp);
            rename("/tmp/inkling_frame.tmp","/tmp/inkling_frame");
            fprintf(stderr,"[inklingfb] grabbed %dx%d bpl=%d fmt=%d (%ld bytes)\n",w,h,bpl,fmt,nb);
        }
    } else fprintf(stderr,"[inklingfb] grab: bad image %dx%d nb=%ld\n",w,h,nb);
    if(p_qimg_dtor) p_qimg_dtor(qi);               // free the grabbed copy
}

// EVERY Qt touch happens here, on the GUI thread, single-threaded — the worker
// thread never calls into Qt at all. Two device freezes came from worker-thread Qt
// access (grabWindow off-thread, property reads racing the GUI): don't move any of
// this back onto the worker.
static const char *TRIGGERS[] = {
    "/tmp/inklingfb_clear",
    "/tmp/inkling_probe", "/tmp/inkling_seldelete",
    "/tmp/inkling_selinfo", "/tmp/inkling_selrect", "/tmp/inkling_spinlayer",
    "/tmp/inkling_tool", 0
};
static void gui_process(void){   // GUI THREAD ONLY
    if(access("/tmp/inkling_tool", F_OK)==0){
        char t[8]={0}; FILE *tf=fopen("/tmp/inkling_tool","r");
        if(tf){ if(!fgets(t,sizeof t,tf)) t[0]=0; fclose(tf); }
        unlink("/tmp/inkling_tool");
        g_tool_req = (t[0]=='s') ? 2 : 1;
        gui_set_tool();
    }
    if(access("/tmp/inklingfb_clear",  F_OK)==0){ clear_page();       unlink("/tmp/inklingfb_clear"); }
    if(access("/tmp/inkling_probe",    F_OK)==0){ probe_selection();  unlink("/tmp/inkling_probe"); }
    if(access("/tmp/inkling_seldelete",F_OK)==0){ selection_delete_native(); unlink("/tmp/inkling_seldelete"); }
    if(access("/tmp/inkling_selinfo",  F_OK)==0){ selection_info();        unlink("/tmp/inkling_selinfo"); }
    if(access("/tmp/inkling_selrect",  F_OK)==0){ select_rect();      unlink("/tmp/inkling_selrect"); }
    if(access("/tmp/inkling_spinlayer",F_OK)==0){ spin_layer();       unlink("/tmp/inkling_spinlayer"); }
    // keep the Convert button present (no-op when the menu is absent or already has it)
    if(p_qcomp_ctor) gui_add_convert_button();
}

// The worker thread watches for trigger files and posts the GUI job. The ONE Qt call
// it makes itself is grab_screen: QQuickWindow::grabWindow ran panel-safe from this
// thread for a whole day of use, whereas ONE grab from the GUI thread mid-animation
// WEDGED the e-ink flush pipeline (panel froze at the spinner while the scene lived
// on — 2026-07-07 14:10). Empirical beats documented here; do not move it back.
// Grabs and GUI jobs are kept from overlapping: no grab while a job is pending, no
// job posted while a grab runs.
static volatile int g_grabbing;
static void* watcher(void* _){
    (void)_;
    int tick = 0;
    for(;;){
        if(access("/tmp/inkling_grab", F_OK)==0 && !g_gui_job){
            g_grabbing = 1;
            grab_screen();
            unlink("/tmp/inkling_grab");
            g_grabbing = 0;
        }
        int want = ((tick % 25) == 5);            // button-injection heartbeat (~3s)
        for(int i=0; !want && TRIGGERS[i]; i++) want = (access(TRIGGERS[i], F_OK)==0);
        if(want && !g_gui_job && !g_grabbing && p_invokeimpl){
            void *app = p_qapp_self ? *p_qapp_self : 0;
            if(app){ g_gui_job = gui_process; p_invokeimpl(app, g_exec_slot, 2, 0); }
        }
        tick++;
        usleep(120000);
    }
    return 0;
}

void _xovi_construct(void){
    p_invoke      = (invoke_fn) dlsym(RTLD_DEFAULT,"_ZN11QMetaObject12invokeMethodEP7QObjectPKcN2Qt14ConnectionTypeE22QGenericReturnArgument16QGenericArgumentS7_S7_S7_S7_S7_S7_S7_S7_S7_");
    p_className   = (classname_fn) dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject9classNameEv");
    p_findchildren= (findchildren_fn) dlsym(RTLD_DEFAULT,"_Z23qt_qFindChildren_helperPK7QObjectRK11QMetaObjectP5QListIPvE6QFlagsIN2Qt15FindChildOptionEE");
    p_allwindows  = (allwindows_fn) dlsym(RTLD_DEFAULT,"_ZN15QGuiApplication10allWindowsEv");
    p_childitems  = (childitems_fn) dlsym(RTLD_DEFAULT,"_ZNK10QQuickItem10childItemsEv");
    p_qproperty   = (qproperty_fn) dlsym(RTLD_DEFAULT,"_ZNK7QObject8propertyEPKc");
    p_propcount   = (propcount_fn) dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject13propertyCountEv");
    p_property    = (property_fn)  dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject8propertyEi");
    p_propname    = (propname_fn)  dlsym(RTLD_DEFAULT,"_ZNK13QMetaProperty4nameEv");
    p_proptype    = (proptype_fn)  dlsym(RTLD_DEFAULT,"_ZNK13QMetaProperty8typeNameEv");
    p_methodcount = (methodcount_fn) dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject11methodCountEv");
    p_method      = (method_fn)      dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject6methodEi");
    p_methodsig   = (methodsig_fn)   dlsym(RTLD_DEFAULT,"_ZNK11QMetaMethod15methodSignatureEv");
    p_bconstdata  = (bconstdata_fn)  dlsym(RTLD_DEFAULT,"_ZNK10QByteArray9constDataEv");
    p_grabwindow  = (grabwindow_fn)  dlsym(RTLD_DEFAULT,"_ZN12QQuickWindow10grabWindowEv");
    p_qimg_bits   = (qimg_bits_fn)   dlsym(RTLD_DEFAULT,"_ZNK6QImage9constBitsEv");
    p_qimg_w      = (qimg_int_fn)    dlsym(RTLD_DEFAULT,"_ZNK6QImage5widthEv");
    p_qimg_h      = (qimg_int_fn)    dlsym(RTLD_DEFAULT,"_ZNK6QImage6heightEv");
    p_qimg_bpl    = (qimg_int_fn)    dlsym(RTLD_DEFAULT,"_ZNK6QImage12bytesPerLineEv");
    p_qimg_fmt    = (qimg_int_fn)    dlsym(RTLD_DEFAULT,"_ZNK6QImage6formatEv");
    p_qimg_size   = (qimg_size_fn)   dlsym(RTLD_DEFAULT,"_ZNK6QImage11sizeInBytesEv");
    p_qimg_dtor   = (qimg_dtor_fn)   dlsym(RTLD_DEFAULT,"_ZN6QImageD1Ev");
    p_methodtype  = (methodtype_fn)  dlsym(RTLD_DEFAULT,"_ZNK11QMetaMethod10methodTypeEv");
    p_v_tobool    = (v_tobool_fn)    dlsym(RTLD_DEFAULT,"_ZNK8QVariant6toBoolEv");
    p_v_toint     = (v_toint_fn)     dlsym(RTLD_DEFAULT,"_ZNK8QVariant5toIntEPb");
    p_v_todouble  = (v_todouble_fn)  dlsym(RTLD_DEFAULT,"_ZNK8QVariant8toDoubleEPb");
    p_v_torectf   = (v_torectf_fn)   dlsym(RTLD_DEFAULT,"_ZNK8QVariant7toRectFEv");
    p_v_topointf  = (v_topointf_fn)  dlsym(RTLD_DEFAULT,"_ZNK8QVariant8toPointFEv");
    p_v_tosizef   = (v_tosizef_fn)   dlsym(RTLD_DEFAULT,"_ZNK8QVariant7toSizeFEv");
    p_v_dtor      = (v_dtor_fn)      dlsym(RTLD_DEFAULT,"_ZN8QVariantD1Ev");
    p_invokeimpl  = (invokeimpl_fn)  dlsym(RTLD_DEFAULT,"_ZN11QMetaObject16invokeMethodImplEP7QObjectPN9QtPrivate15QSlotObjectBaseEN2Qt14ConnectionTypeEPv");
    p_qapp_self   = (void**)         dlsym(RTLD_DEFAULT,"_ZN16QCoreApplication4selfE");
    p_qmlengine   = (qmlengine_fn)   dlsym(RTLD_DEFAULT,"_Z9qmlEnginePK7QObject");
    p_qmlcontext  = (qmlcontext_fn)  dlsym(RTLD_DEFAULT,"_Z10qmlContextPK7QObject");
    p_qcomp_ctor  = (qcomp_ctor_fn)  dlsym(RTLD_DEFAULT,"_ZN13QQmlComponentC1EP10QQmlEngineP7QObject");
    p_qcomp_setdata=(qcomp_setdata_fn)dlsym(RTLD_DEFAULT,"_ZN13QQmlComponent7setDataERK10QByteArrayRK4QUrl");
    p_qcomp_create= (qcomp_create_fn)dlsym(RTLD_DEFAULT,"_ZN13QQmlComponent6createEP11QQmlContext");
    p_qcomp_dtor  = (qcomp_dtor_fn)  dlsym(RTLD_DEFAULT,"_ZN13QQmlComponentD1Ev");
    p_qba_ctor    = (qba_ctor_fn)    dlsym(RTLD_DEFAULT,"_ZN10QByteArrayC1EPKci");
    p_qurl_ctor   = (qurl_ctor_fn)   dlsym(RTLD_DEFAULT,"_ZN4QUrlC1Ev");
    p_qurl_dtor   = (qurl_dtor_fn)   dlsym(RTLD_DEFAULT,"_ZN4QUrlD1Ev");
    p_setprop     = (setprop_fn)     dlsym(RTLD_DEFAULT,"_ZN7QObject11setPropertyEPKcRK8QVariant");
    p_qvar_i      = (qvar_i_fn)      dlsym(RTLD_DEFAULT,"_ZN8QVariantC1Ei");
    p_qvar_d      = (qvar_d_fn)      dlsym(RTLD_DEFAULT,"_ZN8QVariantC1Ed");
    p_setparentitem=(setparentitem_fn)dlsym(RTLD_DEFAULT,"_ZN10QQuickItem13setParentItemEPS_");
    p_setparent   = (setparent_fn)   dlsym(RTLD_DEFAULT,"_ZN7QObject9setParentEPS_");
    p_enumcount   = (enumcount_fn)   dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject15enumeratorCountEv");
    p_enumerator  = (enumerator_fn)  dlsym(RTLD_DEFAULT,"_ZNK11QMetaObject10enumeratorEi");
    p_enumname    = (enumname_fn)    dlsym(RTLD_DEFAULT,"_ZNK9QMetaEnum4nameEv");
    p_enumkeycount= (enumkeycount_fn)dlsym(RTLD_DEFAULT,"_ZNK9QMetaEnum8keyCountEv");
    p_enumkey     = (enumkey_fn)     dlsym(RTLD_DEFAULT,"_ZNK9QMetaEnum3keyEi");
    p_enumvalue   = (enumvalue_fn)   dlsym(RTLD_DEFAULT,"_ZNK9QMetaEnum5valueEi");
    g_qobj_smo    = dlsym(RTLD_DEFAULT,"_ZN7QObject16staticMetaObjectE");
    unlink("/tmp/inklingfb_clear"); unlink("/tmp/inkling_probe"); unlink("/tmp/inkling_grab");
    unlink("/tmp/inkling_selrect"); unlink("/tmp/inkling_tool"); unlink("/tmp/inkling_spinlayer");  // never act on a stale trigger during startup
    fprintf(stderr, "[inklingfb] loaded (clear + selection probe)\n");
    pthread_t t; pthread_create(&t, NULL, watcher, NULL);
}
