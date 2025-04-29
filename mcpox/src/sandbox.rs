// Sandbox.  Mocking what we might want

struct MyServer {}

/// Config options for loading CSV files.
///
/// Other than the paths, these are all optional and the system will attempt to auto-detect
/// reasonable values.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CsvOptions {
    /// Paths to one or more CSV files to load.
    ///
    /// Multiple files are supported in the case where they all have the same schema and should be
    /// loaded and queried as a single table.  This might happen for example if the data is
    /// partitioned across CSV files, or the CSV files are from logs that are rotated on some
    /// interval.
    paths: Vec<PathBuf>,
}

struct CsvInfo {
    delimiter: String,
    quote_char: String,
    // ...
    columns: Vec<CsvColumnInfo>,
}

impl MyServer {
    // Resources:
    // - Schema for each open data source
    // - Metadata for each open data source
    // - ! I don't think there's value in a resource that can be read in order to obtain the data
    // itself.  There are filesystem MCPs if the datasource is that simple.
    // - Reference guide to the DuckDB SQL dialect

    /// Without attaching a CSV data source, inspect it and attempt to deduce information about
    /// it's format and schema.  This isn't required in order to import the CSV data source, but
    /// it can be useful to validate the format in advance.
    #[tool]
    async fn sniff_csv_data_source(&self, path: PathBuf, options: CsvOptions) -> Result<CsvInfo, McpError> {
        // NOTE: See https://duckdb.org/docs/stable/guides/performance/file_formats#loading-many-small-csv-files
        // for a cool bit of detail.  `sniff_csv` includes a `Prompt` column which generates the
        // `read_csv` statement with all of the auto-detected options hard-coded.  For many small
        // files this is more performant.
        todo!()
    }

    /// Load data from CSV file(s) into a new table in the system.
    ///
    /// The data in the CSV files will be copied into the temporary DuckDB database associated with
    /// the session.
    #[tool]
    async fn load_csv_data_source(&self, path: PathBuf, options: CsvOptions) -> Result<Uri, McpError> {
        // Part of this involves making a new resource available, which is the data source
        todo!()
    }

    /// Interrogate a previously opened data source to get information about its schema
    ///
    /// NOTE: there should be a resource created that contains this same schema.  Need to
    /// experiment with MCP clients to see if they're smart enough to load the resource or if they
    /// need to be given a tool to do it.
    #[tool]
    async fn get_data_source_schema(&self, uri: Uri) -> Result<Schema, McpError> {
        todo!()
    }

    /// Sample a few values of each of the columns in a previously-opened data source to provide
    /// more insight into the kind of data that it contains.
    #[tool]
    async fn sample_data_source(&self, uri: Uri) -> Result<Vec<SampleData>, McpError> {
        todo!()
    }

    /// Use the DuckDB SQL dialect to query the data source.
    ///
    /// It's important that errors here are as detail-rich as possible because the model will
    /// almost certainly fuck up the query somehow, and needs to be able to figure out what's
    /// wrong.
    ///
    /// For example once when Claude Code was stupidly hallucinating, and invoked one of its tools
    /// incorrectly, this is what the error was produced by the tool:
    ///
    /// ● Update
    ///    Error: InputValidationError: Edit failed due to the following issues:
    ///    The required parameter `old_string` is missing
    ///    The required parameter `new_string` is missing
    ///    An unexpected parameter `offset` was provided
    ///    An unexpected parameter `limit` was provided
    /// 
    /// Note all of that painstaking detail.  That's not an accident; it helps a retarded LLM get
    /// back on track.  A huge part of making MCP Servers that work well is going to be the
    /// descriptions on the various elements like tools and params, and especially construction of
    /// meaningful error messages when things do not work right.
    #[tool]
    async fn query_data_source(&self, uri: Uri, query: String) -> Result<QueryResults, McpError> {
        todo!()
    }
}

trait McpServer {
    /// Args passed to a new server that may or may not be useful:
    /// - transport-specific information
    /// - handle for sending notifications about tool and resource changes
    async fn init(&mut self, transport: (), server_handle: ()) -> Result<()>;

    /// Event when a new client connects, for initialization of per-client state
    /// Clients need to have unique IDs for this reason.
    ///
    /// This is *before* processing the client's initialize request.  So we don't yet know the
    /// client's capabilities.  There might not even be any value in this event
    async fn client_connected(&self, client: ()) -> Result<ServerCapabilities>;

    /// Client has sent the initialize message with its capabilities.
    /// The client object will include information about the capabilities the client sent
    ///
    /// The server must response with its capabilities.
    async fn client_initialized(&self, client: ()) -> Result<ServerCapabilities>;

    /// A new session is started.  A client might connect multiple times w/ the same session id;
    /// that would fire client_connected each time, but session_started only once
    async fn session_started(&self, client: (), session_id: ()) -> Result<()>;

    /// TODO whether this should be in the trait or not.  The implementation would need to be
    /// provided at the JSON-RPC level, given a specific request ID find the future for that
    /// specific request and cancel it in the JoinSet that we use for pending operations.
    ///
    /// In the MCP spec cancellation is a notification, there's no response needed, and
    /// cancellation is best-effort.  If this succeeds, the pending request should silently be
    /// removed from the set, with no response provided.
    ///
    /// On the MCP client side this would also need to remove the outstanding request so that the
    /// client doesn't keep waiting for it, and on that end the future representing the outstanding
    /// request would need to get some kind of cancellation error back.
    async fn cancel_request(&self, request_id: ()) -> Result<()>;

    // listResources, listTools, blah blah blah
    //
    // Note that at the trait level, these lists are dynamic and can be selectively varied
    // depending on the client.  For example, some tools might require the availability of sampling
    // on the client, and thus be conditionally present or absent depending on that capability.
    //
    // So there needs to be a way to raise the changed notification
    //
    // At the convenience macro level, tools are hard-coded (maybe we'll have the ability to set
    // conditional expressions that determine whether or not a given tool should be exposed to a
    // client).
    async fn list_tools(&self, session: (), client: (), cursor: ()) -> Result<ListToolsResponse>;
    async fn list_resources(&self, session: (), client: (), cursor: ()) -> Result<ListResourcesResponse>;
    async fn list_prompts(&self, session: (), client: (), cursor: ()) -> Result<ListPromptsResponse>;

    /// If the tool fails, this still returns success, and the error is represented in the response
    /// message
    ///
    /// If a progress token is passed then progress reporting is requested.
    async fn call_tool(
        &self,
        session: (),
        client: (),
        tool: String,
        params: (),
        progress: Option<ProgressToken>,
    ) -> Result<CallToolResponse>;

    /// ResourceContent is an enum that represents either text or binary data.
    ///
    /// If a progress token is passed then progress reporting is requested.
    async fn read_resource(
        &self,
        session: (),
        client: (),
        resource: String,
        progress: Option<ProgressToken>,
    ) -> Result<ResourceContent>;
}

struct ServerHandle {}

impl ServerHandle {
    pub async fn notify_tools_changed(&self);
    pub async fn notify_tool_changed(&self, tool: String);
    pub async fn notify_resources_changed(&self);
    pub async fn notify_resource_changed(&self, tool: String);
    pub async fn notify_prompts_changed(&self);
}

/// According to the spec, any request (from context I gather it's methods) can request progress by
/// passing a progress token.  Then whoever is doing the request can report progress.
///
/// I have no idea how widely this is actually implemented in MCP servers or clients.
struct ProgressToken {}

impl ProgressToken {
    /// Per the spec, progres must be monotonically increasing.  Progress and total MAY be floating
    /// point but for our implementation we'll just unconditionally use FP.
    pub async fn report(
        &self,
        progress: double,
        total: double,
        message: impl Into<Option<String>>,
    ) -> Result<()> {
        todo!()
    }
}
